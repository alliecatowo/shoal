//! shoal: a typed, journaled, capability-leashed shell.
//!
//! The workspace used to be ~24 crates; each is now a module of this one.

pub mod adapters;
pub mod ast;
pub mod auth;
pub mod cli;
pub mod config;
pub mod doctor;
pub mod eval;
pub mod exec;
pub mod history;
pub mod host;
pub mod journal;
pub mod kernel;
pub mod leash;
pub mod lsp;
pub mod mcp;
pub mod paths;
pub mod picker;
pub mod prompt;
pub mod proto;
pub mod reef;
pub mod secret;
pub mod syntax;
pub mod value;
pub mod wasm;
