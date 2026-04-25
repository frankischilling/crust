//! Chatterino-compatible filter expression language.
//!
//! Mirrors `chatterino2-master/src/controllers/filters/lang/` but written in
//! Rust. Expressions are tokenized ([`lexer`]), parsed into an [`Expression`]
//! AST ([`parser`]), statically type-checked (`types::synthesize_type`), and
//! evaluated against a [`Context`] (`eval::evaluate`).
//!
//! Typical use:
//! ```
//! use crust_core::filters::{parse, MESSAGE_TYPING_CONTEXT, synthesize_type, Context, Value, evaluate};
//!
//! let expr = parse("author.subbed && message.content contains \"gg\"").unwrap();
//! assert!(synthesize_type(&expr, &MESSAGE_TYPING_CONTEXT).is_ok());
//!
//! let mut ctx = Context::new();
//! ctx.insert("author.subbed".into(), Value::Bool(true));
//! ctx.insert("message.content".into(), Value::Str("gg ez".into()));
//! assert_eq!(evaluate(&expr, &ctx), Value::Bool(true));
//! ```

pub mod ast;
pub mod context;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod types;

pub use ast::{BinOp, Expression, Span, UnOp};
pub use context::{build_message_context, MESSAGE_TYPING_CONTEXT};
pub use eval::evaluate;
pub use lexer::{tokenize, LexError, Token};
pub use parser::{parse, ParseError};
pub use types::{synthesize_type, Context, Type, TypeError, TypingContext, Value};
