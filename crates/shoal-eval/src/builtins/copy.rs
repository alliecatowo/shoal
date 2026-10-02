//! Bounded, effect-free recursive-copy planning followed by execution.
//!
//! Inventory retains source and destination capabilities before any mutation;
//! admission enforces resource and cross-job bounds; execution consumes only
//! the admitted capabilities.

mod admission;
mod execution;
mod path_policy;
mod plan;
mod policy;

use super::admission::{MAX_RETAINED_BYTES, MAX_VALUES};
use shoal_value::ErrorVal;

pub(super) use plan::CopyPlan;

const MAX_COPY_DEPTH: usize = 64;

fn work_limit(message: impl Into<String>) -> ErrorVal {
    ErrorVal::new("builtin_work_limit", message)
        .with_hint("copy a narrower tree or split the operation into bounded subtrees")
}

#[cfg(test)]
mod tests;
