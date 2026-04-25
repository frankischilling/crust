//! Runtime evaluator for the filter expression AST.
//!
//! Mirrors `chatterino2-master/src/controllers/filters/lang/expressions/*`
//! but intentionally lenient: identifiers that are missing from the
//! [`Context`] resolve to a default value for their inferred operator
//! operand (so a filter referencing `flags.automod` still works on
//! platforms where that flag isn't tracked).

use crate::filters::ast::{BinOp, Expression, UnOp};
use crate::filters::types::{Context, Value};

/// Evaluate `expr` against `ctx`.
///
/// The evaluator never panics and never returns a `Result`; a runtime
/// issue (divide-by-zero, type mismatch that slipped past the static
/// checker, missing identifier) resolves to a sensible `false`/empty
/// result, matching Chatterino's behavior.
pub fn evaluate(expr: &Expression, ctx: &Context) -> Value {
    match expr {
        Expression::Literal { value, .. } => value.clone(),
        Expression::Identifier { name, .. } => ctx
            .get(name)
            .cloned()
            .unwrap_or(Value::Bool(false)),
        Expression::List { items, .. } => {
            Value::List(items.iter().map(|e| evaluate(e, ctx)).collect())
        }
        Expression::Unary { op, rhs, .. } => {
            let v = evaluate(rhs, ctx);
            match op {
                UnOp::Not => Value::Bool(!v.truthy()),
            }
        }
        Expression::Binary { op, lhs, rhs, .. } => eval_binop(*op, lhs, rhs, ctx),
    }
}

fn eval_binop(op: BinOp, lhs: &Expression, rhs: &Expression, ctx: &Context) -> Value {
    // Short-circuit logical ops.
    match op {
        BinOp::And => {
            let l = evaluate(lhs, ctx);
            if !l.truthy() {
                return Value::Bool(false);
            }
            let r = evaluate(rhs, ctx);
            return Value::Bool(r.truthy());
        }
        BinOp::Or => {
            let l = evaluate(lhs, ctx);
            if l.truthy() {
                return Value::Bool(true);
            }
            let r = evaluate(rhs, ctx);
            return Value::Bool(r.truthy());
        }
        _ => {}
    }

    let l = evaluate(lhs, ctx);
    let r = evaluate(rhs, ctx);
    match op {
        BinOp::Eq => Value::Bool(values_equal(&l, &r)),
        BinOp::Neq => Value::Bool(!values_equal(&l, &r)),
        BinOp::Lt => int_cmp(&l, &r, |a, b| a < b),
        BinOp::Gt => int_cmp(&l, &r, |a, b| a > b),
        BinOp::Lte => int_cmp(&l, &r, |a, b| a <= b),
        BinOp::Gte => int_cmp(&l, &r, |a, b| a >= b),
        BinOp::Contains => Value::Bool(eval_contains(&l, &r)),
        BinOp::StartsWith => Value::Bool(eval_starts_with(&l, &r)),
        BinOp::EndsWith => Value::Bool(eval_ends_with(&l, &r)),
        BinOp::Match => eval_match(&l, &r),
        BinOp::Plus => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
            (Value::Str(a), Value::Str(b)) => Value::Str(format!("{a}{b}")),
            _ => Value::Bool(false),
        },
        BinOp::Minus => match (l.as_int(), r.as_int()) {
            (Some(a), Some(b)) => Value::Int(a.wrapping_sub(b)),
            _ => Value::Bool(false),
        },
        BinOp::Multiply => match (l.as_int(), r.as_int()) {
            (Some(a), Some(b)) => Value::Int(a.wrapping_mul(b)),
            _ => Value::Bool(false),
        },
        BinOp::Divide => match (l.as_int(), r.as_int()) {
            (Some(a), Some(b)) if b != 0 => Value::Int(a.wrapping_div(b)),
            _ => Value::Bool(false),
        },
        BinOp::Mod => match (l.as_int(), r.as_int()) {
            (Some(a), Some(b)) if b != 0 => Value::Int(a.wrapping_rem(b)),
            _ => Value::Bool(false),
        },
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Regex(x), Value::Regex(y)) => x.as_str() == y.as_str(),
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_equal(a, b))
        }
        // Cross-type int/bool comparison
        (Value::Int(i), Value::Bool(b)) | (Value::Bool(b), Value::Int(i)) => *i == i64::from(*b),
        // String <=> Int coercion: match Chatterino's tolerant comparison
        (Value::Str(s), Value::Int(i)) | (Value::Int(i), Value::Str(s)) => {
            s.parse::<i64>().map(|n| n == *i).unwrap_or(false)
        }
        _ => false,
    }
}

fn int_cmp(a: &Value, b: &Value, f: impl Fn(i64, i64) -> bool) -> Value {
    match (a.as_int(), b.as_int()) {
        (Some(x), Some(y)) => Value::Bool(f(x, y)),
        _ => Value::Bool(false),
    }
}

fn eval_contains(haystack: &Value, needle: &Value) -> bool {
    match (haystack, needle) {
        (Value::Str(s), Value::Str(n)) => s.contains(n.as_str()),
        (Value::List(items), other) => items.iter().any(|v| values_equal(v, other)),
        _ => false,
    }
}

fn eval_starts_with(haystack: &Value, needle: &Value) -> bool {
    match (haystack, needle) {
        (Value::Str(s), Value::Str(n)) => s.starts_with(n.as_str()),
        (Value::List(items), other) => items.first().map(|v| values_equal(v, other)).unwrap_or(false),
        _ => false,
    }
}

fn eval_ends_with(haystack: &Value, needle: &Value) -> bool {
    match (haystack, needle) {
        (Value::Str(s), Value::Str(n)) => s.ends_with(n.as_str()),
        (Value::List(items), other) => items.last().map(|v| values_equal(v, other)).unwrap_or(false),
        _ => false,
    }
}

/// `match` against a `Regex` literal -> Bool, or against a `{regex, int}`
/// specifier -> String of the captured group (empty on no-match).
fn eval_match(haystack: &Value, pattern: &Value) -> Value {
    let text = match haystack {
        Value::Str(s) => s.as_str(),
        _ => return Value::Bool(false),
    };
    match pattern {
        Value::Regex(re) => Value::Bool(re.is_match(text)),
        Value::List(items) if items.len() == 2 => {
            let (re, idx) = match (&items[0], &items[1]) {
                (Value::Regex(r), Value::Int(i)) => (r, *i),
                _ => return Value::Bool(false),
            };
            if let Some(caps) = re.captures(text) {
                if idx < 0 {
                    return Value::Str(String::new());
                }
                let group = caps.get(idx as usize).map(|m| m.as_str()).unwrap_or("");
                Value::Str(group.to_owned())
            } else {
                Value::Str(String::new())
            }
        }
        _ => Value::Bool(false),
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::parser::parse;

    fn ctx(pairs: &[(&str, Value)]) -> Context {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn eval_ticket_expression_subbed_gg() {
        let expr = parse("author.subscriber && message.content contains \"gg\"").unwrap();
        let c = ctx(&[
            ("author.subscriber", Value::Bool(true)),
            ("message.content", Value::Str("gg ez".into())),
        ]);
        assert_eq!(evaluate(&expr, &c), Value::Bool(true));
    }

    #[test]
    fn eval_ticket_expression_non_sub_does_not_fire() {
        let expr = parse("author.subscriber && message.content contains \"gg\"").unwrap();
        let c = ctx(&[
            ("author.subscriber", Value::Bool(false)),
            ("message.content", Value::Str("gg ez".into())),
        ]);
        assert_eq!(evaluate(&expr, &c), Value::Bool(false));
    }

    #[test]
    fn eval_list_contains_string() {
        let expr = parse("author.badges contains \"moderator\"").unwrap();
        let c = ctx(&[(
            "author.badges",
            Value::List(vec![
                Value::Str("subscriber".into()),
                Value::Str("moderator".into()),
            ]),
        )]);
        assert_eq!(evaluate(&expr, &c), Value::Bool(true));
    }

    #[test]
    fn eval_or_short_circuits() {
        let expr = parse("true_var || absent.thing").unwrap();
        let c = ctx(&[("true_var", Value::Bool(true))]);
        assert_eq!(evaluate(&expr, &c), Value::Bool(true));
    }

    #[test]
    fn eval_regex_match() {
        let expr = parse("message.content match r\"^gg\"").unwrap();
        let c = ctx(&[("message.content", Value::Str("gg ez".into()))]);
        assert_eq!(evaluate(&expr, &c), Value::Bool(true));
    }

    #[test]
    fn eval_arithmetic() {
        let expr = parse("(1 + 2) * 4").unwrap();
        assert_eq!(evaluate(&expr, &Context::new()), Value::Int(12));
    }

    #[test]
    fn eval_string_concat() {
        let expr = parse("\"a\" + \"b\"").unwrap();
        assert_eq!(
            evaluate(&expr, &Context::new()),
            Value::Str("ab".into())
        );
    }

    #[test]
    fn eval_divide_by_zero_is_false() {
        let expr = parse("1 / 0").unwrap();
        assert_eq!(evaluate(&expr, &Context::new()), Value::Bool(false));
    }

    #[test]
    fn eval_unary_not() {
        let expr = parse("!flags.highlighted").unwrap();
        let c = ctx(&[("flags.highlighted", Value::Bool(false))]);
        assert_eq!(evaluate(&expr, &c), Value::Bool(true));
    }

    #[test]
    fn eval_missing_identifier_is_false() {
        let expr = parse("flags.absent").unwrap();
        assert_eq!(evaluate(&expr, &Context::new()), Value::Bool(false));
    }

    #[test]
    fn eval_match_group_returns_string() {
        let expr = parse("message.content match {r\"^(\\w+)\", 1}").unwrap();
        let c = ctx(&[("message.content", Value::Str("hello world".into()))]);
        assert_eq!(evaluate(&expr, &c), Value::Str("hello".into()));
    }

    #[test]
    fn eval_starts_and_ends() {
        let a = parse("message.content startswith \"he\"").unwrap();
        let b = parse("message.content endswith \"lo\"").unwrap();
        let c = ctx(&[("message.content", Value::Str("hello".into()))]);
        assert_eq!(evaluate(&a, &c), Value::Bool(true));
        assert_eq!(evaluate(&b, &c), Value::Bool(true));
    }
}
