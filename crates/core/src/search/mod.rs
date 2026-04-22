//! Cross-channel message search: predicate engine and parser.
//!
//! Pure, no UI or async dependencies. Used by the global search popup
//! in `crust_ui` and any future search consumers.

pub mod parser;
pub mod predicate;

pub use parser::{parse, ParseOutcome};
pub use predicate::{matches, FlagKind, Predicate};
