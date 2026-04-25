//! Recursive-descent parser for the filter expression language.
//!
//! Precedence (low -> high):
//! 1. `||`
//! 2. `&&`
//! 3. unary `!`
//! 4. comparisons (`==`, `!=`, `<`, `<=`, `>`, `>=`, `contains`, `startswith`,
//!    `endswith`, `match`)
//! 5. additive (`+`, `-`)
//! 6. multiplicative (`*`, `/`, `%`)
//! 7. primary (literals, identifiers, `(...)`, `{...}`)
//!
//! Errors carry a [`Span`] so callers can highlight the exact position of
//! the offending token in the UI.

use std::sync::Arc;

use thiserror::Error;

use crate::filters::ast::{BinOp, Expression, Span, UnOp};
use crate::filters::lexer::{tokenize, LexError, Spanned, Token};
use crate::filters::types::Value;

/// Parse error with a source span for UI highlighting.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("lex error: {0}")]
    Lex(LexError),
    #[error("unexpected token {got}; expected {expected}")]
    UnexpectedToken {
        got: String,
        expected: String,
        span: Span,
    },
    #[error("unexpected end of input; expected {expected}")]
    UnexpectedEof { expected: String, span: Span },
    #[error("unexpected trailing token {got}")]
    TrailingToken { got: String, span: Span },
    #[error("invalid regular expression: {message}")]
    InvalidRegex { message: String, span: Span },
    #[error("empty expression")]
    Empty { span: Span },
}

impl ParseError {
    pub fn span(&self) -> Span {
        match self {
            ParseError::Lex(e) => e.span(),
            ParseError::UnexpectedToken { span, .. }
            | ParseError::UnexpectedEof { span, .. }
            | ParseError::TrailingToken { span, .. }
            | ParseError::InvalidRegex { span, .. }
            | ParseError::Empty { span } => *span,
        }
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError::Lex(e)
    }
}

/// Parse `input` into an [`Expression`] tree.
pub fn parse(input: &str) -> Result<Expression, ParseError> {
    let toks = tokenize(input)?;
    if toks.is_empty() {
        return Err(ParseError::Empty { span: Span::DUMMY });
    }
    let mut parser = Parser::new(toks);
    let expr = parser.parse_expression()?;
    if let Some(tok) = parser.peek().cloned() {
        return Err(ParseError::TrailingToken {
            got: tok.token.label(),
            span: tok.span,
        });
    }
    Ok(expr)
}

struct Parser {
    toks: Vec<Spanned<Token>>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Spanned<Token>>) -> Self {
        Self { toks, pos: 0 }
    }

    fn peek(&self) -> Option<&Spanned<Token>> {
        self.toks.get(self.pos)
    }

    fn advance(&mut self) -> Option<Spanned<Token>> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn last_span(&self) -> Span {
        self.toks
            .last()
            .map(|t| t.span)
            .unwrap_or(Span::DUMMY)
    }

    fn parse_expression(&mut self) -> Result<Expression, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expression, ParseError> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek().map(|t| &t.token), Some(Token::Or)) {
            let op_tok = self.advance().unwrap();
            let rhs = self.parse_and()?;
            let span = op_tok.span.merge(lhs.span()).merge(rhs.span());
            lhs = Expression::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expression, ParseError> {
        let mut lhs = self.parse_unary()?;
        while matches!(self.peek().map(|t| &t.token), Some(Token::And)) {
            let op_tok = self.advance().unwrap();
            let rhs = self.parse_unary()?;
            let span = op_tok.span.merge(lhs.span()).merge(rhs.span());
            lhs = Expression::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expression, ParseError> {
        if matches!(self.peek().map(|t| &t.token), Some(Token::Not)) {
            let op_tok = self.advance().unwrap();
            let rhs = self.parse_unary()?;
            let span = op_tok.span.merge(rhs.span());
            return Ok(Expression::Unary {
                op: UnOp::Not,
                rhs: Box::new(rhs),
                span,
            });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expression, ParseError> {
        let mut lhs = self.parse_additive()?;
        while let Some(op) = self.peek_comparison_op() {
            self.advance();
            let rhs = self.parse_additive()?;
            let span = lhs.span().merge(rhs.span());
            lhs = Expression::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn peek_comparison_op(&self) -> Option<BinOp> {
        match self.peek().map(|t| &t.token)? {
            Token::Eq => Some(BinOp::Eq),
            Token::Neq => Some(BinOp::Neq),
            Token::Lt => Some(BinOp::Lt),
            Token::Gt => Some(BinOp::Gt),
            Token::Lte => Some(BinOp::Lte),
            Token::Gte => Some(BinOp::Gte),
            Token::Contains => Some(BinOp::Contains),
            Token::StartsWith => Some(BinOp::StartsWith),
            Token::EndsWith => Some(BinOp::EndsWith),
            Token::Match => Some(BinOp::Match),
            _ => None,
        }
    }

    fn parse_additive(&mut self) -> Result<Expression, ParseError> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek().map(|t| &t.token) {
                Some(Token::Plus) => BinOp::Plus,
                Some(Token::Minus) => BinOp::Minus,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            let span = lhs.span().merge(rhs.span());
            lhs = Expression::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expression, ParseError> {
        let mut lhs = self.parse_primary()?;
        loop {
            let op = match self.peek().map(|t| &t.token) {
                Some(Token::Multiply) => BinOp::Multiply,
                Some(Token::Divide) => BinOp::Divide,
                Some(Token::Mod) => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_primary()?;
            let span = lhs.span().merge(rhs.span());
            lhs = Expression::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_primary(&mut self) -> Result<Expression, ParseError> {
        let spanned = match self.advance() {
            Some(t) => t,
            None => {
                return Err(ParseError::UnexpectedEof {
                    expected: "expression".into(),
                    span: self.last_span(),
                })
            }
        };
        match spanned.token {
            Token::Int(n) => Ok(Expression::Literal {
                value: Value::Int(n),
                span: spanned.span,
            }),
            Token::Str(s) => Ok(Expression::Literal {
                value: Value::Str(s),
                span: spanned.span,
            }),
            Token::Regex {
                pattern,
                case_insensitive,
            } => {
                let re = regex::RegexBuilder::new(&pattern)
                    .case_insensitive(case_insensitive)
                    .build()
                    .map_err(|e| ParseError::InvalidRegex {
                        message: e.to_string(),
                        span: spanned.span,
                    })?;
                Ok(Expression::Literal {
                    value: Value::Regex(Arc::new(re)),
                    span: spanned.span,
                })
            }
            Token::Ident(name) => Ok(Expression::Identifier {
                name,
                span: spanned.span,
            }),
            Token::Lp => {
                let inner = self.parse_expression()?;
                match self.advance() {
                    Some(Spanned {
                        token: Token::Rp, ..
                    }) => Ok(inner),
                    Some(other) => Err(ParseError::UnexpectedToken {
                        got: other.token.label(),
                        expected: "`)`".into(),
                        span: other.span,
                    }),
                    None => Err(ParseError::UnexpectedEof {
                        expected: "`)`".into(),
                        span: self.last_span(),
                    }),
                }
            }
            Token::LBrace => self.parse_list(spanned.span),
            // unary - for negative literals: desugar `-<int>` to literal negative.
            Token::Minus => {
                let rhs = self.parse_primary()?;
                if let Expression::Literal {
                    value: Value::Int(n),
                    span: rhs_span,
                } = rhs
                {
                    return Ok(Expression::Literal {
                        value: Value::Int(-n),
                        span: spanned.span.merge(rhs_span),
                    });
                }
                // Otherwise treat as 0 - rhs.
                let zero = Expression::Literal {
                    value: Value::Int(0),
                    span: spanned.span,
                };
                let span = spanned.span.merge(rhs.span());
                Ok(Expression::Binary {
                    op: BinOp::Minus,
                    lhs: Box::new(zero),
                    rhs: Box::new(rhs),
                    span,
                })
            }
            other => Err(ParseError::UnexpectedToken {
                got: other.label(),
                expected: "expression".into(),
                span: spanned.span,
            }),
        }
    }

    fn parse_list(&mut self, open_span: Span) -> Result<Expression, ParseError> {
        let mut items = Vec::new();
        if matches!(self.peek().map(|t| &t.token), Some(Token::RBrace)) {
            let close = self.advance().unwrap();
            return Ok(Expression::List {
                items,
                span: open_span.merge(close.span),
            });
        }
        loop {
            let item = self.parse_expression()?;
            items.push(item);
            match self.peek().map(|t| t.token.clone()) {
                Some(Token::Comma) => {
                    self.advance();
                    continue;
                }
                Some(Token::RBrace) => {
                    let close = self.advance().unwrap();
                    return Ok(Expression::List {
                        items,
                        span: open_span.merge(close.span),
                    });
                }
                Some(other) => {
                    return Err(ParseError::UnexpectedToken {
                        got: other.label(),
                        expected: "`,` or `}`".into(),
                        span: self.peek().map(|t| t.span).unwrap_or(open_span),
                    });
                }
                None => {
                    return Err(ParseError::UnexpectedEof {
                        expected: "`}`".into(),
                        span: self.last_span(),
                    });
                }
            }
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_binop(expr: &Expression, op: BinOp) {
        match expr {
            Expression::Binary { op: o, .. } => assert_eq!(*o, op),
            other => panic!("expected {:?}, got {:?}", op, other),
        }
    }

    #[test]
    fn parse_simple_identifier() {
        let e = parse("author.name").unwrap();
        match e {
            Expression::Identifier { name, .. } => assert_eq!(name, "author.name"),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_and_has_higher_precedence_than_or() {
        let e = parse("a && b || c").unwrap();
        assert_binop(&e, BinOp::Or);
        if let Expression::Binary { lhs, .. } = e {
            assert_binop(&lhs, BinOp::And);
        }
    }

    #[test]
    fn parse_unary_not() {
        let e = parse("!a").unwrap();
        match e {
            Expression::Unary { op, .. } => assert_eq!(op, UnOp::Not),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_parentheses_override_precedence() {
        let e = parse("a && (b || c)").unwrap();
        assert_binop(&e, BinOp::And);
        if let Expression::Binary { rhs, .. } = e {
            assert_binop(&rhs, BinOp::Or);
        }
    }

    #[test]
    fn parse_contains_string() {
        let e = parse("message.content contains \"gg\"").unwrap();
        assert_binop(&e, BinOp::Contains);
    }

    #[test]
    fn parse_match_regex() {
        let e = parse("message.content match r\"^hi\"").unwrap();
        assert_binop(&e, BinOp::Match);
    }

    #[test]
    fn parse_list_literal() {
        let e = parse("{\"a\", \"b\"}").unwrap();
        match e {
            Expression::List { items, .. } => assert_eq!(items.len(), 2),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_arithmetic_precedence() {
        let e = parse("1 + 2 * 3").unwrap();
        assert_binop(&e, BinOp::Plus);
        if let Expression::Binary { rhs, .. } = e {
            assert_binop(&rhs, BinOp::Multiply);
        }
    }

    #[test]
    fn parse_missing_close_paren() {
        let err = parse("(a && b").unwrap_err();
        matches!(err, ParseError::UnexpectedEof { .. });
    }

    #[test]
    fn parse_trailing_token_rejected() {
        let err = parse("a b").unwrap_err();
        assert!(matches!(err, ParseError::TrailingToken { .. }));
        assert!(err.span().start > 0);
    }

    #[test]
    fn parse_ticket_expression() {
        let e = parse("author.subscriber && message.content contains \"gg\"").unwrap();
        assert_binop(&e, BinOp::And);
    }

    #[test]
    fn parse_error_reports_span_for_invalid_list() {
        let err = parse("{1, }").unwrap_err();
        assert!(err.span().start > 0);
    }

    #[test]
    fn parse_negative_literal() {
        let e = parse("-5").unwrap();
        match e {
            Expression::Literal { value, .. } => match value {
                Value::Int(n) => assert_eq!(n, -5),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn parse_empty_input() {
        let err = parse("   ").unwrap_err();
        assert!(matches!(err, ParseError::Empty { .. }));
    }

    #[test]
    fn parse_invalid_regex_rejected() {
        let err = parse("r\"[unclosed\"").unwrap_err();
        assert!(matches!(err, ParseError::InvalidRegex { .. }));
    }
}
