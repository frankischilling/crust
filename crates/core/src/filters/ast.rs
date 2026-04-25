//! AST for the filter expression language.
//!
//! Each node carries a [`Span`] pointing back into the source string for
//! error reporting. The AST is intentionally small and close to the
//! Chatterino layout:
//!
//! - [`Expression::Literal`] wraps a pre-parsed [`crate::filters::Value`]
//!   (int, string, regex).
//! - [`Expression::Identifier`] is a typed identifier (`message.content`,
//!   `author.badges`, ...); resolved at eval time against the context map.
//! - [`Expression::List`] is `{a, b, c}`.
//! - [`Expression::Unary`] and [`Expression::Binary`] cover the operator
//!   nodes.

use crate::filters::types::Value;

/// Half-open byte range into the source string, plus a 1-based line/column
/// for nicer error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
}

impl Span {
    pub fn new(start: usize, end: usize, line: u32, col: u32) -> Self {
        Self {
            start,
            end,
            line,
            col,
        }
    }

    /// Zero-width span at the very start of the input. Used as a fallback
    /// for errors that can't be localized to a real token.
    pub const DUMMY: Span = Span {
        start: 0,
        end: 0,
        line: 1,
        col: 1,
    };

    /// Merge two spans; the merged span covers both ranges.
    pub fn merge(self, other: Span) -> Span {
        let start = self.start.min(other.start);
        let end = self.end.max(other.end);
        // Keep the earliest-starting position for line/col.
        let (line, col) = if self.start <= other.start {
            (self.line, self.col)
        } else {
            (other.line, other.col)
        };
        Span {
            start,
            end,
            line,
            col,
        }
    }
}

/// Unary operators. Chatterino only has logical negation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
}

/// Binary operators. Grouped loosely by category in the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    // logical
    And,
    Or,
    // comparison
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    // text
    Contains,
    StartsWith,
    EndsWith,
    Match,
    // arithmetic
    Plus,
    Minus,
    Multiply,
    Divide,
    Mod,
}

impl BinOp {
    /// Human-readable label for debug messages.
    pub fn label(self) -> &'static str {
        match self {
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::Eq => "==",
            BinOp::Neq => "!=",
            BinOp::Lt => "<",
            BinOp::Gt => ">",
            BinOp::Lte => "<=",
            BinOp::Gte => ">=",
            BinOp::Contains => "contains",
            BinOp::StartsWith => "startswith",
            BinOp::EndsWith => "endswith",
            BinOp::Match => "match",
            BinOp::Plus => "+",
            BinOp::Minus => "-",
            BinOp::Multiply => "*",
            BinOp::Divide => "/",
            BinOp::Mod => "%",
        }
    }
}

/// A filter expression AST node.
#[derive(Debug, Clone)]
pub enum Expression {
    /// Concrete value literal (int, string, regex).
    Literal { value: Value, span: Span },
    /// Reference to a typed variable in the context.
    Identifier { name: String, span: Span },
    /// List expression: `{a, b, c}`.
    List { items: Vec<Expression>, span: Span },
    /// Unary operation: `!expr`.
    Unary {
        op: UnOp,
        rhs: Box<Expression>,
        span: Span,
    },
    /// Binary operation.
    Binary {
        op: BinOp,
        lhs: Box<Expression>,
        rhs: Box<Expression>,
        span: Span,
    },
}

impl Expression {
    /// Span that covers this entire expression.
    pub fn span(&self) -> Span {
        match self {
            Expression::Literal { span, .. }
            | Expression::Identifier { span, .. }
            | Expression::List { span, .. }
            | Expression::Unary { span, .. }
            | Expression::Binary { span, .. } => *span,
        }
    }
}
