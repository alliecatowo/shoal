//! Effect attribution for `glob(..)` values.

use super::attribution::str_literal;
use super::*;
use crate::plan_effects::push_effect;

impl Evaluator {
    /// A glob value is enumerated by `.expand()` and by iteration, so
    /// constructing one claims a read of its concrete root directory (or is
    /// opaque when the pattern is dynamic). Without this, an agent with a
    /// narrow `fs.read` could list any directory.
    pub(super) fn plan_glob_effect(&self, args: &Args, out: &mut Vec<Effect>) {
        match args.pos.first().and_then(str_literal) {
            Some(pattern) => push_effect(
                out,
                Effect::FsRead {
                    paths: vec![self.plan_abs(&glob_concrete_root(&pattern))],
                },
            ),
            None => push_effect(out, Effect::Opaque),
        }
    }
}

/// The longest leading run of components with no glob metacharacters
/// (`/a/b/*.txt` -> `/a/b`; `*.txt` -> `.`).
fn glob_concrete_root(pattern: &str) -> String {
    let mut root = Vec::new();
    for component in pattern.split('/') {
        if component.contains(['*', '?', '[', '{']) {
            break;
        }
        root.push(component);
    }
    // The last literal component of a metachar-free pattern is a file; its
    // parent is still covered by reading the pattern's own path.
    let joined = root.join("/");
    if joined.is_empty() && !pattern.starts_with('/') {
        ".".into()
    } else if joined.is_empty() {
        "/".into()
    } else {
        joined
    }
}
