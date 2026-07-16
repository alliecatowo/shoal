//! The one canonical builtin command-head registry.
//!
//! Deciding "is this token a builtin command head?" is a lexical/syntactic
//! classification, and every consumer of the answer already depends on
//! `shoal-syntax`: the evaluator's dispatch (`shoal-eval`), the shell's
//! completer/highlighter (`shoal`), and the LSP (`shoal-lsp`). So the list lives
//! here in the leaf `shoal-syntax` crate — the LSP needn't pull the whole
//! evaluator in just to know the command-head vocabulary. `shoal-eval` keeps its
//! dispatch logic (`builtins::run`/`dispatch`, `eval_command`'s special-head
//! guards); it just sources the *name list* from this single source of truth
//! (re-exporting these helpers so its call sites stay tidy).

use std::sync::LazyLock;

/// Structured builtins dispatched by `shoal-eval`'s `builtins::run`/`dispatch` —
/// the fs / env / sleep family that produces a typed `Value` from raw CMD words.
/// This is the set [`is_builtin`] gates the generic dispatch on; keeping it
/// separate from [`SPECIAL_HEADS`] is load-bearing (a special head like `cd` must
/// NOT route to `dispatch`, which only knows these fourteen).
const NAMES: &[&str] = &[
    "echo", "ls", "cat", "mkdir", "touch", "cp", "mv", "rm", "stat", "which", "env", "sleep",
    "head", "ln",
];

/// Command heads intercepted directly in `eval_command` (session navigation, job
/// control, journal/undo, plan verbs, `source`/`run`) — dispatched there by name
/// to their own methods, NOT via `dispatch`. Together with [`NAMES`] this is the
/// complete builtin command-head vocabulary exposed by [`builtin_names`] and
/// honored by `shoal-eval`'s `Evaluator::is_command_name`. Keep this in lockstep
/// with the `if call.head == "…"` guards in `shoal-eval`'s `command.rs`.
const SPECIAL_HEADS: &[&str] = &[
    "cd", "pushd", "popd", "dirs", "j", "jump", "pwd", "exit", "quit", "source", "run", "jobs",
    "interact", "assert", "open", "save", "reef", "undo", "journal", "history", "plan", "apply",
    "explain",
];

/// The one canonical builtin registry: every name that resolves as a builtin
/// command head (structured builtins ∪ special heads), sorted and deduped. This
/// is THE source of truth — the completer, highlighter, and LSP all consume it
/// (via [`builtin_names`]) instead of hand-maintaining their own drifting copies.
static BUILTIN_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let mut v: Vec<&'static str> = NAMES.iter().chain(SPECIAL_HEADS).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
});

/// The canonical, sorted, deduped list of builtin command-head names (structured
/// builtins ∪ the special heads intercepted in `eval_command`). The shell
/// (completion/highlighting) and the LSP derive their builtin vocabulary from
/// eval's own registry rather than a stale hand-copy that silently drifts.
pub fn builtin_names() -> &'static [&'static str] {
    &BUILTIN_NAMES
}

/// A structured builtin (routes to `shoal-eval`'s `builtins::run`)? Gates the
/// generic dispatch in `eval_command`; distinct from [`is_special_head`].
pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

/// A command head special-cased directly in `eval_command` (not via `run`)?
/// Consumed by `shoal-eval`'s `Evaluator::is_command_name` so its notion of
/// "resolves as a builtin command" stays tied to the registry data here.
pub fn is_special_head(name: &str) -> bool {
    SPECIAL_HEADS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtin_registry_is_complete_sorted_and_deduped() {
        let names = builtin_names();
        // Every builtin command head — structured ∪ special — must be present.
        for expected in [
            "cd", "pushd", "popd", "dirs", "history", "journal", "jobs", "exit", "quit", "plan",
            "apply", "explain", "undo", "reef", "save", "assert", "interact", "open", "run",
            "source", "j", "jump", "pwd", "echo", "ls", "cat", "which", "env", "head", "ln",
        ] {
            assert!(
                names.contains(&expected),
                "registry is missing builtin `{expected}`"
            );
        }
        // `clear` was a bogus entry in the highlighter's old hand-list — it is
        // NOT a shoal builtin (only ever an external on PATH).
        assert!(
            !names.contains(&"clear"),
            "`clear` is not a builtin and must not be in the registry"
        );
        // The public list is sorted and deduped so consumers can binary-search.
        let mut want = names.to_vec();
        want.sort_unstable();
        want.dedup();
        assert_eq!(
            names,
            want.as_slice(),
            "registry must be sorted and deduped"
        );
        // Structured builtins and special heads are disjoint (`which` is the
        // only name reachable both ways, and it lives in NAMES) — the union
        // count is exactly the two lists' lengths.
        assert_eq!(names.len(), NAMES.len() + SPECIAL_HEADS.len());
        // The canonical set is exactly 37 names (14 structured + 23 special) —
        // pin the size so a stray addition/removal is caught here.
        assert_eq!(names.len(), 37);
    }

    #[test]
    fn membership_helpers_agree_with_the_lists() {
        assert!(is_builtin("ls"));
        assert!(!is_builtin("cd"));
        assert!(is_special_head("cd"));
        assert!(!is_special_head("ls"));
        // Every name is reachable through exactly one of the two predicates.
        for name in builtin_names() {
            assert!(
                is_builtin(name) ^ is_special_head(name),
                "`{name}` must be reachable through exactly one predicate"
            );
        }
    }
}
