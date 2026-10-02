//! Principal policy: TOML-loaded per-principal grants, effect/plan
//! evaluation, and the lowering of filesystem grants into a concrete
//! [`crate::SandboxPolicy`] for one child spawn.

use crate::effects::{Effect, Plan, Reversibility};
use crate::enforce::{FsSandbox, NetPolicy, ProcessLimits, SandboxPolicy};
use glob::Pattern;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

mod grants;
mod model;
mod parsing;

#[cfg(test)]
pub(crate) use grants::grant_root;
pub use model::{
    AutoApply, OpaqueMode, POLICY_MAX_ASSIGNMENTS, POLICY_MAX_BYTES, POLICY_MAX_GRANT_BYTES,
    POLICY_MAX_GRANTS_PER_KIND, POLICY_MAX_NESTING, POLICY_MAX_PRINCIPALS, Policy, PrincipalPolicy,
    Verdict,
};
pub use parsing::{PolicyLoadError, PolicyParseError};

use grants::*;
use model::PolicyDoc;
use parsing::*;

#[cfg(test)]
#[path = "policy/tests.rs"]
mod input_tests;
