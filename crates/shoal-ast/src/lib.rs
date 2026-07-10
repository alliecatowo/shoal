//! shoal-ast — the canonical AST for the shoal shell.
//!
//! This is the single syntax vocabulary shared by the parser (`shoal-syntax`),
//! the evaluator (`shoal-eval`), the journal, and the wire protocol. Every node
//! carries a byte-offset [`Span`] into the source it was parsed from, and the
//! whole tree serializes to the canonical JSON encoding via serde (`kind`-tagged).

pub mod ast;
pub mod span;

pub use ast::*;
pub use span::Span;
