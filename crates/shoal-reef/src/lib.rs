//! **reef** — tool resolution, ripped from the root PATH fossil (`site/content/internals/reef-resolution.md`).
//!
//! A name resolves through **scopes**, not directories: project `.reef.toml`
//! (nearest wins) → user `[reef]` scope → provider-discovered system/ambient
//! tools. Resolution is a pure, journaled function; PATH becomes an *output*
//! synthesized for legacy children, never an input the shell lives in.
//!
//! # The pipeline
//!
//! 1. [`ScopeChain::discover`] walks up from a `cwd`, collecting `.reef.toml`
//!    (and read-only foreign `mise.toml` / `.tool-versions`) manifests into an
//!    ordered, nearest-first chain, then appends the user scope. It is a pure
//!    function of `(cwd, filesystem)` — no activation, no hooks, no env
//!    mutation, ever. `cd` re-scopes the *next* resolution; nothing else
//!    changes.
//! 2. A [`Resolver`] — an ordered [`Provider`] stack — resolves one tool name
//!    against that chain, consulting and updating a [`Lockfile`] under a
//!    [`Policy`] (interactive auto-lock vs. script hard-error), and returns a
//!    [`Resolution`] carrying the full [`ResolutionReport`] that `which`
//!    renders: the winning scope, version, provider, content hash, and the
//!    complete scope chain annotated with each scope's outcome.
//! 3. [`synth_path`] turns a resolved binding set into a synthesized `PATH`
//!    for legacy children that themselves search PATH (build scripts spawning
//!    `cc`, `npm` spawning `node`) — a content-addressed view directory of
//!    symlinks, reused across calls with the same bindings.
//! 4. [`resolve_runner`] answers "how do I run `./script.py`" by mapping the
//!    file extension (falling back to a `#!` shebang sniff) to a tool name
//!    plus an argv template; the named tool is then resolved through step 2
//!    like any other name.
//!
//! Errors are values, not panics: [`ReefError`] carries a stable
//! [`ReefCode`] (`reef_unlocked`, `reef_drift`, `reef_conflict`,
//! `reef_not_found`, `reef_provider`) plus a human message and an optional
//! fix-it hint.
//!
//! # Example
//!
//! ```
//! use shoal_reef::{Lockfile, Policy, Resolver, ScopeChain};
//!
//! // Pure function of (cwd, filesystem) — no activation, no env mutation.
//! let cwd = std::env::current_dir().unwrap();
//! let chain = ScopeChain::discover(&cwd, None);
//!
//! let resolver = Resolver::with_defaults();
//! let mut lock = Lockfile::new();
//! let mut notices = Vec::new();
//!
//! match resolver.resolve("node", &chain, &mut lock, Policy::Interactive, &mut |n| {
//!     notices.push(n.name.clone());
//! }) {
//!     Ok(resolution) => {
//!         // `resolution.report` is what `which node` renders: scope, version,
//!         // provider, hash, and the full nearest-first chain.
//!         println!("node -> {} via {}", resolution.version, resolution.provider);
//!     }
//!     Err(e) => {
//!         // e.g. reef_not_found when nothing on this machine provides `node`.
//!         eprintln!("{e}");
//!     }
//! }
//! ```

pub mod error;
pub mod hashcache;
mod input;
pub mod lock;
pub mod manifest;
pub mod provider;
pub mod report;
pub mod resolve;
pub mod runner;
pub mod scope;
pub mod timestamp;
pub mod version;
pub mod view;

pub use error::{ReefCode, ReefError, ReefResult};
pub use input::{
    REEF_MANIFEST_MAX_BYTES, REEF_MANIFEST_MAX_NESTING, REEF_MAX_RUNNERS, REEF_MAX_SCOPES,
    REEF_MAX_STRING_BYTES, REEF_MAX_TOOLS,
};
pub use lock::{LockEntry, LockError, Lockfile};
pub use manifest::{ManifestError, ManifestKind, ReefManifest, ToolReq};
pub use provider::{
    Candidate, CandidateDiscovery, MAX_DISCOVERY_CANDIDATES, MAX_DISCOVERY_RETAINED_BYTES,
    Provider, ProviderCtx, ProviderError,
};
pub use report::{ResolutionReport, ScopeDecision};
pub use resolve::{LockNotice, Policy, ProbeExecution, Resolution, Resolver};
pub use runner::{Invocation, RunnerTable, resolve_runner, sniff_shebang};
pub use scope::{ChainKey, ScopeChain, ScopeEntry};
pub use version::{Constraint, Version};
pub use view::{
    Binding, SynthView, ViewConfig, bindings_hash, default_system_tail, default_view_root,
    synth_path,
};
