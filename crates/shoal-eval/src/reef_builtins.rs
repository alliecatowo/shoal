//! `which`/`reef` builtins layered on reef resolution (site/content/internals/reef-resolution.md).
//!
//! Split out of [`crate::reef`] (see that module's doc for the split
//! rationale); see [`crate::reef_resolve`] for the scope-chain/resolver
//! mechanics these commands call into.

use super::*;
use crate::builtins::admission::{OutputBudget, OutputValues, table_record};
use std::collections::HashSet;
use std::io::Read as _;

use shoal_reef::hashcache::HashCache;
use shoal_reef::{
    ManifestKind, Policy, ProbeExecution, ProviderCtx, ReefCode, ReefError, ResolutionReport,
    ScopeChain,
};
use shoal_syntax::commands::CommandSource;

mod commands;
mod support;
mod which;

use support::*;

#[cfg(test)]
#[path = "reef_builtins/tests.rs"]
mod tests;
