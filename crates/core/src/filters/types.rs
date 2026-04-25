//! Types and values for the filter expression language.
//!
//! Mirrors `chatterino2-master/src/controllers/filters/lang/Types.hpp`.
//!
//! [`Type`] describes what kind of value an expression produces; runtime
//! [`Value`]s carry that data. Static type-checking via [`synthesize_type`]
//! walks the AST with a [`TypingContext`] (identifier -> declared type) and
//! either returns the expression's result type or a positional [`TypeError`].

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

use crate::filters::ast::{BinOp, Expression, Span, UnOp};

/// Declared type of an expression.
///
/// `List` is a heterogeneous list; `StringList` means every element is a
/// String (used for e.g. `author.badges`). `MatchingSpecifier` is a
/// two-element `{regex, int}` list that describes a `match ... group N`
/// operation (same as Chatterino).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Bool,
    Int,
    String,
    Regex,
    List,
    StringList,
    MatchingSpecifier,
}

impl Type {
    pub fn label(self) -> &'static str {
        match self {
            Type::Bool => "Bool",
            Type::Int => "Int",
            Type::String => "String",
            Type::Regex => "Regex",
            Type::List => "List",
            Type::StringList => "StringList",
            Type::MatchingSpecifier => "MatchingSpecifier",
        }
    }

    /// True when this type is assignable to `Type::List`.
    pub fn is_list(self) -> bool {
        matches!(
            self,
            Type::List | Type::StringList | Type::MatchingSpecifier
        )
    }
}

/// A runtime value produced by evaluating an expression.
#[derive(Debug, Clone)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(String),
    Regex(Arc<regex::Regex>),
    List(Vec<Value>),
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Regex(a), Value::Regex(b)) => a.as_str() == b.as_str(),
            (Value::List(a), Value::List(b)) => a == b,
            // Cross-type comparison: allow Int <-> Bool via truthy coercion
            // to match Chatterino's lenient eval semantics.
            (Value::Bool(a), Value::Int(b)) | (Value::Int(b), Value::Bool(a)) => {
                i64::from(*a) == *b
            }
            _ => false,
        }
    }
}

impl Value {
    pub fn type_of(&self) -> Type {
        match self {
            Value::Bool(_) => Type::Bool,
            Value::Int(_) => Type::Int,
            Value::Str(_) => Type::String,
            Value::Regex(_) => Type::Regex,
            Value::List(items) => {
                if items.iter().all(|v| matches!(v, Value::Str(_))) {
                    Type::StringList
                } else if items.len() == 2
                    && matches!(items[0], Value::Regex(_))
                    && matches!(items[1], Value::Int(_))
                {
                    Type::MatchingSpecifier
                } else {
                    Type::List
                }
            }
        }
    }

    /// Truthiness test used by `&&`, `||`, and `!`.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Regex(_) => true,
            Value::List(items) => !items.is_empty(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Bool(b) => Some(i64::from(*b)),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> bool {
        self.truthy()
    }
}

/// Runtime identifier -> value map.
pub type Context = HashMap<String, Value>;

/// Compile-time identifier -> declared type map.
pub type TypingContext = HashMap<String, Type>;

/// Error returned by [`synthesize_type`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TypeError {
    #[error("unknown identifier `{name}`")]
    UnknownIdentifier { name: String, span: Span },
    #[error("type mismatch: operator `{op}` expects {expected} but got {actual}")]
    TypeMismatch {
        op: String,
        expected: String,
        actual: String,
        span: Span,
    },
    #[error("list elements must share a compatible type")]
    ListElementMismatch { span: Span },
}

impl TypeError {
    pub fn span(&self) -> Span {
        match self {
            TypeError::UnknownIdentifier { span, .. }
            | TypeError::TypeMismatch { span, .. }
            | TypeError::ListElementMismatch { span } => *span,
        }
    }
}

/// Recursively compute the result type of `expr`, walking the AST.
pub fn synthesize_type(
    expr: &Expression,
    typing: &TypingContext,
) -> Result<Type, TypeError> {
    match expr {
        Expression::Literal { value, .. } => Ok(value.type_of()),
        Expression::Identifier { name, span } => typing
            .get(name)
            .copied()
            .ok_or_else(|| TypeError::UnknownIdentifier {
                name: name.clone(),
                span: *span,
            }),
        Expression::List { items, span } => {
            if items.is_empty() {
                return Ok(Type::List);
            }
            let mut types = Vec::with_capacity(items.len());
            for it in items {
                types.push(synthesize_type(it, typing)?);
            }
            // Chatterino's MatchingSpecifier pattern: {Regex, Int}.
            if types.len() == 2 && types[0] == Type::Regex && types[1] == Type::Int {
                return Ok(Type::MatchingSpecifier);
            }
            if types.iter().all(|t| *t == Type::String) {
                return Ok(Type::StringList);
            }
            // Heterogeneous but still valid - just a List.
            // (We don't error on mixed types so users can compose freely.)
            let _ = span;
            Ok(Type::List)
        }
        Expression::Unary { op, rhs, span } => {
            let t = synthesize_type(rhs, typing)?;
            match op {
                UnOp::Not => {
                    if !matches!(t, Type::Bool | Type::Int) {
                        return Err(TypeError::TypeMismatch {
                            op: "!".into(),
                            expected: "Bool".into(),
                            actual: t.label().into(),
                            span: *span,
                        });
                    }
                    Ok(Type::Bool)
                }
            }
        }
        Expression::Binary {
            op,
            lhs,
            rhs,
            span,
        } => {
            let lt = synthesize_type(lhs, typing)?;
            let rt = synthesize_type(rhs, typing)?;
            type_of_binop(*op, lt, rt, *span)
        }
    }
}

fn type_of_binop(op: BinOp, lt: Type, rt: Type, span: Span) -> Result<Type, TypeError> {
    use BinOp::*;
    match op {
        And | Or => {
            if !is_boolish(lt) || !is_boolish(rt) {
                return Err(TypeError::TypeMismatch {
                    op: op.label().into(),
                    expected: "Bool".into(),
                    actual: format!("{} and {}", lt.label(), rt.label()),
                    span,
                });
            }
            Ok(Type::Bool)
        }
        Eq | Neq => Ok(Type::Bool),
        Lt | Gt | Lte | Gte => {
            if !matches!(lt, Type::Int | Type::Bool) || !matches!(rt, Type::Int | Type::Bool) {
                return Err(TypeError::TypeMismatch {
                    op: op.label().into(),
                    expected: "Int".into(),
                    actual: format!("{} and {}", lt.label(), rt.label()),
                    span,
                });
            }
            Ok(Type::Bool)
        }
        Contains => {
            let ok = match (lt, rt) {
                (Type::String, Type::String) => true,
                (Type::List, _) => true,
                (Type::StringList, Type::String) => true,
                (Type::MatchingSpecifier, _) => true,
                _ => false,
            };
            if !ok {
                return Err(TypeError::TypeMismatch {
                    op: "contains".into(),
                    expected: "String or List on the left".into(),
                    actual: format!("{} and {}", lt.label(), rt.label()),
                    span,
                });
            }
            Ok(Type::Bool)
        }
        StartsWith | EndsWith => {
            let ok = match (lt, rt) {
                (Type::String, Type::String) => true,
                (Type::StringList, Type::String) => true,
                (Type::List, _) => true,
                _ => false,
            };
            if !ok {
                return Err(TypeError::TypeMismatch {
                    op: op.label().into(),
                    expected: "String/StringList on the left".into(),
                    actual: format!("{} and {}", lt.label(), rt.label()),
                    span,
                });
            }
            Ok(Type::Bool)
        }
        Match => {
            if lt != Type::String {
                return Err(TypeError::TypeMismatch {
                    op: "match".into(),
                    expected: "String on the left".into(),
                    actual: lt.label().into(),
                    span,
                });
            }
            match rt {
                Type::Regex => Ok(Type::Bool),
                Type::MatchingSpecifier => Ok(Type::String),
                _ => Err(TypeError::TypeMismatch {
                    op: "match".into(),
                    expected: "Regex or {Regex, Int}".into(),
                    actual: rt.label().into(),
                    span,
                }),
            }
        }
        Plus => {
            match (lt, rt) {
                (Type::Int, Type::Int) => Ok(Type::Int),
                (Type::String, Type::String) => Ok(Type::String),
                _ => Err(TypeError::TypeMismatch {
                    op: "+".into(),
                    expected: "Int+Int or String+String".into(),
                    actual: format!("{} and {}", lt.label(), rt.label()),
                    span,
                }),
            }
        }
        Minus | Multiply | Divide | Mod => {
            if !matches!(lt, Type::Int | Type::Bool) || !matches!(rt, Type::Int | Type::Bool) {
                return Err(TypeError::TypeMismatch {
                    op: op.label().into(),
                    expected: "Int".into(),
                    actual: format!("{} and {}", lt.label(), rt.label()),
                    span,
                });
            }
            Ok(Type::Int)
        }
    }
}

fn is_boolish(t: Type) -> bool {
    matches!(
        t,
        Type::Bool | Type::Int | Type::String | Type::List | Type::StringList
    )
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::lexer::Token;
    use crate::filters::parser::parse;

    fn typing() -> TypingContext {
        let mut t = TypingContext::new();
        t.insert("author.subbed".into(), Type::Bool);
        t.insert("author.subscriber".into(), Type::Bool);
        t.insert("author.name".into(), Type::String);
        t.insert("author.badges".into(), Type::StringList);
        t.insert("message.content".into(), Type::String);
        t.insert("message.length".into(), Type::Int);
        t.insert("flags.highlighted".into(), Type::Bool);
        t
    }

    #[test]
    fn type_check_ticket_acceptance() {
        let expr = parse("author.subscriber && message.content contains \"gg\"").unwrap();
        assert_eq!(synthesize_type(&expr, &typing()).unwrap(), Type::Bool);
    }

    #[test]
    fn string_and_int_fails_type_check() {
        let expr = parse("\"gg\" + 1").unwrap();
        let err = synthesize_type(&expr, &typing()).unwrap_err();
        assert!(matches!(err, TypeError::TypeMismatch { .. }));
    }

    #[test]
    fn unknown_identifier_reported_with_span() {
        let expr = parse("bogus.name").unwrap();
        let err = synthesize_type(&expr, &typing()).unwrap_err();
        match err {
            TypeError::UnknownIdentifier { name, span } => {
                assert_eq!(name, "bogus.name");
                assert_eq!(span.start, 0);
            }
            _ => panic!("wrong err {err:?}"),
        }
    }

    #[test]
    fn list_of_strings_types_as_string_list() {
        let expr = parse("{\"a\", \"b\", \"c\"}").unwrap();
        assert_eq!(synthesize_type(&expr, &typing()).unwrap(), Type::StringList);
    }

    #[test]
    fn matching_specifier_typed() {
        let expr = parse("{r\"hi\", 1}").unwrap();
        assert_eq!(
            synthesize_type(&expr, &typing()).unwrap(),
            Type::MatchingSpecifier
        );
    }

    #[test]
    fn value_truthy() {
        assert!(Value::Bool(true).truthy());
        assert!(!Value::Bool(false).truthy());
        assert!(Value::Int(3).truthy());
        assert!(!Value::Int(0).truthy());
        assert!(Value::Str("x".into()).truthy());
        assert!(!Value::Str(String::new()).truthy());
    }

    // ensure the Token import is exercised by doc-test style (no-op here)
    #[allow(dead_code)]
    fn _compile_check(_t: Token) {}
}
