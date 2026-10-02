//! Hexagonal ports. See `site/content/internals/effects-plans-security.md`
//! and `site/content/internals/intercrate-protocol-contracts.md`.
//!
//! The evaluator (`shoal-eval`) is meant to be the pure domain core, but it
//! historically reached straight into the OS with scattered `std::fs`,
//! `std::process`, `std::time`, and secret-store calls. These traits are the
//! seam: the domain core holds a `dyn Port` for each effect family and the host
//! wires an adapter. The [`StdFs`], [`StdClock`], and [`StdOpener`] adapters
//! defined here perform *exactly* the calls the inline code did, so installing
//! them (the default) is byte-identical to the pre-ports behavior.
//!
//! Adapters that need other workspace crates (`Exec` over `shoal-exec`,
//! `SecretPort` over `shoal-secret`) keep their trait here but implement the
//! `Std*` adapter in `shoal-eval`, so `shoal-value` stays a leaf crate.

mod copy_destination;

mod copy_source;

mod filesystem;

mod fs_metadata;

mod opener;

mod removal_tree;

mod runtime;

mod std_fs;

pub use copy_destination::{FsCopyDestination, FsCopyTarget};

pub use copy_source::{FsCopyChild, FsCopySource};

pub use filesystem::{Fs, FsEntryIdentity, FsFileSnapshot, ReadSeek};

pub use opener::{Opener, StdOpener};

pub use removal_tree::FsRemovalTree;

pub use runtime::{BytesLoad, Clock, ConfigPort, ConfigSnapshot, SecretPort, StdClock};

pub use std_fs::StdFs;
