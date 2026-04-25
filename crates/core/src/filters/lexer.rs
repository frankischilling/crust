//! Tokenizer for the filter expression language.
//!
//! Mirrors `chatterino2-master/src/controllers/filters/lang/Tokenizer.cpp` but
//! produces `Spanned<Token>` values rather than raw strings so the parser can
//! report precise error locations.

use thiserror::Error;

use crate::filters::ast::Span;

/// A single token. Payload-carrying variants (`Str`, `Regex`, `Int`, `Ident`)
/// own their parsed data; operators are unit-like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    // control
    And,
    Or,
    Lp,
    Rp,
    LBrace,
    RBrace,
    Comma,
    // binary
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    Contains,
    StartsWith,
    EndsWith,
    Match,
    // unary
    Not,
    // math
    Plus,
    Minus,
    Multiply,
    Divide,
    Mod,
    // values
    Int(i64),
    Str(String),
    Regex {
        pattern: String,
        case_insensitive: bool,
    },
    Ident(String),
}

impl Token {
    /// Short human-readable label used in parser errors.
    pub fn label(&self) -> String {
        match self {
            Token::And => "`&&`".into(),
            Token::Or => "`||`".into(),
            Token::Lp => "`(`".into(),
            Token::Rp => "`)`".into(),
            Token::LBrace => "`{`".into(),
            Token::RBrace => "`}`".into(),
            Token::Comma => "`,`".into(),
            Token::Eq => "`==`".into(),
            Token::Neq => "`!=`".into(),
            Token::Lt => "`<`".into(),
            Token::Gt => "`>`".into(),
            Token::Lte => "`<=`".into(),
            Token::Gte => "`>=`".into(),
            Token::Contains => "`contains`".into(),
            Token::StartsWith => "`startswith`".into(),
            Token::EndsWith => "`endswith`".into(),
            Token::Match => "`match`".into(),
            Token::Not => "`!`".into(),
            Token::Plus => "`+`".into(),
            Token::Minus => "`-`".into(),
            Token::Multiply => "`*`".into(),
            Token::Divide => "`/`".into(),
            Token::Mod => "`%`".into(),
            Token::Int(n) => format!("integer `{n}`"),
            Token::Str(s) => format!("string \"{s}\""),
            Token::Regex { pattern, .. } => format!("regex `/{pattern}/`"),
            Token::Ident(n) => format!("identifier `{n}`"),
        }
    }
}

/// A token together with its source span.
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    pub token: T,
    pub span: Span,
}

/// Errors raised by [`tokenize`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LexError {
    #[error("unterminated string literal")]
    UnterminatedString { span: Span },
    #[error("unterminated regex literal")]
    UnterminatedRegex { span: Span },
    #[error("invalid integer literal `{text}`")]
    InvalidInt { text: String, span: Span },
    #[error("unexpected character `{ch}`")]
    UnexpectedChar { ch: char, span: Span },
}

impl LexError {
    pub fn span(&self) -> Span {
        match self {
            LexError::UnterminatedString { span }
            | LexError::UnterminatedRegex { span }
            | LexError::InvalidInt { span, .. }
            | LexError::UnexpectedChar { span, .. } => *span,
        }
    }
}

/// Tokenize `input` into a flat stream of [`Spanned`] tokens.
pub fn tokenize(input: &str) -> Result<Vec<Spanned<Token>>, LexError> {
    let mut lexer = Lexer::new(input);
    let mut out = Vec::new();
    while let Some(tok) = lexer.next_token()? {
        out.push(tok);
    }
    Ok(out)
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn current_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_byte(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.src[self.pos..].chars().next()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn make_span(&self, start: usize, start_line: u32, start_col: u32) -> Span {
        Span::new(start, self.pos, start_line, start_col)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.src[self.pos..].chars().next() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Result<Option<Spanned<Token>>, LexError> {
        self.skip_whitespace();
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        let start = self.pos;
        let line = self.line;
        let col = self.col;

        // Regex literal: r"..." or ri"..."
        if self.current_byte() == Some(b'r') {
            let ci = self.peek_byte(1) == Some(b'i') && self.peek_byte(2) == Some(b'"');
            let just_r = self.peek_byte(1) == Some(b'"');
            if ci || just_r {
                // consume prefix
                self.advance(); // r
                if ci {
                    self.advance(); // i
                }
                // opening quote
                self.advance();
                let content_start = self.pos;
                loop {
                    match self.current_byte() {
                        None => {
                            return Err(LexError::UnterminatedRegex {
                                span: self.make_span(start, line, col),
                            })
                        }
                        Some(b'\\') => {
                            // Consume backslash + the following char (e.g. \")
                            self.advance();
                            if self.current_byte().is_some() {
                                self.advance();
                            }
                        }
                        Some(b'"') => {
                            let pattern = self.src[content_start..self.pos]
                                .replace("\\\"", "\"");
                            self.advance(); // consume closing quote
                            let span = self.make_span(start, line, col);
                            return Ok(Some(Spanned {
                                token: Token::Regex {
                                    pattern,
                                    case_insensitive: ci,
                                },
                                span,
                            }));
                        }
                        Some(_) => {
                            self.advance();
                        }
                    }
                }
            }
        }

        // String literal
        if self.current_byte() == Some(b'"') {
            self.advance();
            let content_start = self.pos;
            loop {
                match self.current_byte() {
                    None => {
                        return Err(LexError::UnterminatedString {
                            span: self.make_span(start, line, col),
                        })
                    }
                    Some(b'\\') => {
                        self.advance();
                        if self.current_byte().is_some() {
                            self.advance();
                        }
                    }
                    Some(b'"') => {
                        let raw = &self.src[content_start..self.pos];
                        let value = unescape_string(raw);
                        self.advance();
                        let span = self.make_span(start, line, col);
                        return Ok(Some(Spanned {
                            token: Token::Str(value),
                            span,
                        }));
                    }
                    Some(_) => {
                        self.advance();
                    }
                }
            }
        }

        // Multi-char symbols.
        if let Some(b0) = self.current_byte() {
            // Two-char operators first.
            match (b0, self.peek_byte(1)) {
                (b'&', Some(b'&')) => return Ok(Some(self.consume_n(Token::And, 2, start, line, col))),
                (b'|', Some(b'|')) => return Ok(Some(self.consume_n(Token::Or, 2, start, line, col))),
                (b'=', Some(b'=')) => return Ok(Some(self.consume_n(Token::Eq, 2, start, line, col))),
                (b'!', Some(b'=')) => return Ok(Some(self.consume_n(Token::Neq, 2, start, line, col))),
                (b'<', Some(b'=')) => return Ok(Some(self.consume_n(Token::Lte, 2, start, line, col))),
                (b'>', Some(b'=')) => return Ok(Some(self.consume_n(Token::Gte, 2, start, line, col))),
                _ => {}
            }
            // Single-char symbols.
            match b0 {
                b'(' => return Ok(Some(self.consume_n(Token::Lp, 1, start, line, col))),
                b')' => return Ok(Some(self.consume_n(Token::Rp, 1, start, line, col))),
                b'{' => return Ok(Some(self.consume_n(Token::LBrace, 1, start, line, col))),
                b'}' => return Ok(Some(self.consume_n(Token::RBrace, 1, start, line, col))),
                b',' => return Ok(Some(self.consume_n(Token::Comma, 1, start, line, col))),
                b'<' => return Ok(Some(self.consume_n(Token::Lt, 1, start, line, col))),
                b'>' => return Ok(Some(self.consume_n(Token::Gt, 1, start, line, col))),
                b'!' => return Ok(Some(self.consume_n(Token::Not, 1, start, line, col))),
                b'+' => return Ok(Some(self.consume_n(Token::Plus, 1, start, line, col))),
                b'-' => {
                    // Could be a negative integer literal if followed by a digit,
                    // but Chatterino treats '-' as an operator and parses
                    // negatives as Minus <int>. We match that behavior.
                    return Ok(Some(self.consume_n(Token::Minus, 1, start, line, col)));
                }
                b'*' => return Ok(Some(self.consume_n(Token::Multiply, 1, start, line, col))),
                b'/' => return Ok(Some(self.consume_n(Token::Divide, 1, start, line, col))),
                b'%' => return Ok(Some(self.consume_n(Token::Mod, 1, start, line, col))),
                _ => {}
            }
        }

        // Integer literal
        if self
            .current_byte()
            .map(|b| b.is_ascii_digit())
            .unwrap_or(false)
        {
            while self
                .current_byte()
                .map(|b| b.is_ascii_digit())
                .unwrap_or(false)
            {
                self.advance();
            }
            let text = &self.src[start..self.pos];
            match text.parse::<i64>() {
                Ok(n) => {
                    let span = self.make_span(start, line, col);
                    return Ok(Some(Spanned {
                        token: Token::Int(n),
                        span,
                    }));
                }
                Err(_) => {
                    return Err(LexError::InvalidInt {
                        text: text.to_owned(),
                        span: self.make_span(start, line, col),
                    })
                }
            }
        }

        // Identifier or keyword: [A-Za-z_][A-Za-z0-9_.]*
        if self
            .current_byte()
            .map(|b| b.is_ascii_alphabetic() || b == b'_')
            .unwrap_or(false)
        {
            while self
                .current_byte()
                .map(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
                .unwrap_or(false)
            {
                self.advance();
            }
            let text = &self.src[start..self.pos];
            let span = self.make_span(start, line, col);
            let tok = match text {
                "contains" => Token::Contains,
                "startswith" => Token::StartsWith,
                "endswith" => Token::EndsWith,
                "match" => Token::Match,
                _ => Token::Ident(text.to_owned()),
            };
            return Ok(Some(Spanned { token: tok, span }));
        }

        // Unknown character.
        let ch = self.advance().unwrap_or('\0');
        Err(LexError::UnexpectedChar {
            ch,
            span: self.make_span(start, line, col),
        })
    }

    fn consume_n(
        &mut self,
        token: Token,
        n: usize,
        start: usize,
        line: u32,
        col: u32,
    ) -> Spanned<Token> {
        for _ in 0..n {
            self.advance();
        }
        Spanned {
            token,
            span: self.make_span(start, line, col),
        }
    }
}

fn unescape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(input: &str) -> Vec<Token> {
        tokenize(input)
            .unwrap()
            .into_iter()
            .map(|s| s.token)
            .collect()
    }

    #[test]
    fn tokenizes_basic_operators() {
        assert_eq!(
            toks("a && b || c"),
            vec![
                Token::Ident("a".into()),
                Token::And,
                Token::Ident("b".into()),
                Token::Or,
                Token::Ident("c".into()),
            ]
        );
    }

    #[test]
    fn tokenizes_comparisons() {
        assert_eq!(
            toks("x == 1 != 2 <= 3 >= 4 < 5 > 6"),
            vec![
                Token::Ident("x".into()),
                Token::Eq,
                Token::Int(1),
                Token::Neq,
                Token::Int(2),
                Token::Lte,
                Token::Int(3),
                Token::Gte,
                Token::Int(4),
                Token::Lt,
                Token::Int(5),
                Token::Gt,
                Token::Int(6),
            ]
        );
    }

    #[test]
    fn tokenizes_text_keywords() {
        assert_eq!(
            toks("a contains \"x\" startswith \"y\" endswith \"z\" match r\"re\""),
            vec![
                Token::Ident("a".into()),
                Token::Contains,
                Token::Str("x".into()),
                Token::StartsWith,
                Token::Str("y".into()),
                Token::EndsWith,
                Token::Str("z".into()),
                Token::Match,
                Token::Regex {
                    pattern: "re".into(),
                    case_insensitive: false,
                },
            ]
        );
    }

    #[test]
    fn tokenizes_dotted_identifier() {
        assert_eq!(
            toks("author.badges"),
            vec![Token::Ident("author.badges".into())]
        );
    }

    #[test]
    fn tokenizes_regex_case_insensitive() {
        assert_eq!(
            toks("ri\"Hello\""),
            vec![Token::Regex {
                pattern: "Hello".into(),
                case_insensitive: true,
            }]
        );
    }

    #[test]
    fn tokenizes_string_with_escaped_quote() {
        assert_eq!(
            toks("\"a\\\"b\""),
            vec![Token::Str("a\"b".into())]
        );
    }

    #[test]
    fn unterminated_string_errors() {
        let err = tokenize("\"oops").unwrap_err();
        assert!(matches!(err, LexError::UnterminatedString { .. }));
    }

    #[test]
    fn unterminated_regex_errors() {
        let err = tokenize("r\"oops").unwrap_err();
        assert!(matches!(err, LexError::UnterminatedRegex { .. }));
    }

    #[test]
    fn tokenizes_list_literal() {
        assert_eq!(
            toks("{1, 2, 3}"),
            vec![
                Token::LBrace,
                Token::Int(1),
                Token::Comma,
                Token::Int(2),
                Token::Comma,
                Token::Int(3),
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn tokenizes_parentheses_and_not() {
        assert_eq!(
            toks("!(a)"),
            vec![
                Token::Not,
                Token::Lp,
                Token::Ident("a".into()),
                Token::Rp,
            ]
        );
    }

    #[test]
    fn spans_are_recorded() {
        let toks = tokenize("a && b").unwrap();
        assert_eq!(toks.len(), 3);
        assert_eq!(toks[0].span.start, 0);
        assert_eq!(toks[0].span.end, 1);
        assert_eq!(toks[1].span.start, 2);
        assert_eq!(toks[1].span.end, 4);
        assert_eq!(toks[2].span.start, 5);
        assert_eq!(toks[2].span.end, 6);
    }

    #[test]
    fn line_col_tracking() {
        let toks = tokenize("a\n&& b").unwrap();
        assert_eq!(toks[0].span.line, 1);
        assert_eq!(toks[1].span.line, 2);
        assert_eq!(toks[1].span.col, 1);
    }
}
