//! DSL surface for projects: emitter and parser per
//! `04-dsl-grammar.md`.
//!
//! `emit` walks the in-memory `Project` and produces `.keds` text. `parse`
//! goes the other way, hand-written recursive descent over a stream of
//! tokens from `lex`. The parser supports the subset the emitter produces;
//! features outside that subset return `ParseError::Unsupported(name)` so
//! callers see exactly what's missing.

pub mod emit;
pub mod lex;
pub mod parse;

pub use emit::{project_to_dsl, EmitError};
pub use parse::{parse_project, ParseError};
