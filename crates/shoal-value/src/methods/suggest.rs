//! Did-you-mean hints for the unknown-method fall-through in
//! [`super::dispatch`]. A field-test showed agents burning turns on
//! `.length`/`.to_upper`/`.read_str`-style near-misses that errored with no
//! pointer to the method that does exist.
//!
//! The candidate sets below are harvested from the names `dispatch` actually
//! accepts (the big match in `methods/mod.rs`), grouped by receiver type so we
//! only suggest methods plausible for the receiver — plus, for `path`, the
//! fs-backed methods the evaluator dispatches *before* `call_method`
//! (`shoal-eval/src/expr_access.rs::path_fs_method`), which are just as real
//! to the user. Keep these lists in sync when adding dispatch arms.

use super::*;

/// Collection ops shared by `list`/`table`/`range` (dispatch's `seq`-backed
/// arms plus the receiver-polymorphic ones).
const SEQ_METHODS: &[&str] = &[
    "len",
    "count",
    "is_empty",
    "first",
    "last",
    "collect",
    "stream",
    "tee",
    "map",
    "reduce",
    "fold",
    "where",
    "filter",
    "each",
    "any",
    "all",
    "find",
    "flat_map",
    "sort_by",
    "sort",
    "reverse",
    "uniq",
    "sum",
    "min",
    "max",
    "flatten",
    "enumerate",
    "skip",
    "take",
    "chunks",
    "zip",
    "group",
    "group_by",
    "join",
    "contains",
    "get",
    "json",
    "save",
    "append",
    "feed",
];

const STR_METHODS: &[&str] = &[
    "len",
    "count",
    "is_empty",
    "lines",
    "words",
    "chars",
    "trim",
    "upper",
    "lower",
    "split",
    "starts_with",
    "ends_with",
    "contains",
    "replace",
    "matches",
    "match",
    "parse_int",
    "parse_float",
    "take",
    "skip",
    "reverse",
    "stream",
    "str",
    "display",
    "json",
    "save",
    "append",
    "feed",
];

const RECORD_METHODS: &[&str] = &[
    "keys", "values", "items", "set", "merge", "get", "contains", "len", "count", "is_empty",
    "json", "save", "append", "feed",
];

const NUM_METHODS: &[&str] = &[
    "abs", "round", "floor", "ceil", "str", "display", "json", "save", "append", "feed",
];

/// Pure component accessors (this crate) + the fs-backed set the evaluator
/// dispatches ahead of `call_method`. `feed` is deliberately absent — a bare
/// path is a name, not content (IO.md §1.2).
const PATH_METHODS: &[&str] = &[
    "name",
    "stem",
    "ext",
    "parent",
    "join",
    "abs",
    "read",
    "read_bytes",
    "lines",
    "exists",
    "is_dir",
    "is_file",
    "size",
    "modified",
    "save",
    "append",
    "str",
    "display",
    "json",
];

const TASK_METHODS: &[&str] = &[
    "await",
    "wait",
    "cancel",
    "is_done",
    "suspend",
    "resume",
    "is_suspended",
];

const BYTES_METHODS: &[&str] = &[
    "len", "count", "is_empty", "str", "display", "stream", "json", "save", "append", "feed",
];

/// Quantity/temporal scalars: no unary method surface of their own beyond the
/// universal serializers.
const SCALAR_METHODS: &[&str] = &["json", "save", "append", "feed"];

/// Receiver-polymorphic methods dispatched at the top of
/// [`super::dispatch`] (before the per-type arms), so they never appear in any
/// of the per-receiver `*_METHODS` tables above.
const POLY_METHODS: &[&str] = &["tap", "also"];

/// Every method name the dispatch table accepts, across all receiver types,
/// sorted and deduped. Powers method/field completion after `.` in the shell
/// (and any other surface that needs the flat method vocabulary). Built by
/// unioning the per-receiver tables above so it can't drift from the
/// did-you-mean hint sets — extend a `*_METHODS` list and this list grows too.
static METHOD_NAMES: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&'static str> = SEQ_METHODS
        .iter()
        .chain(STR_METHODS)
        .chain(RECORD_METHODS)
        .chain(NUM_METHODS)
        .chain(PATH_METHODS)
        .chain(TASK_METHODS)
        .chain(BYTES_METHODS)
        .chain(SCALAR_METHODS)
        .chain(POLY_METHODS)
        .copied()
        .collect();
    v.sort_unstable();
    v.dedup();
    v
});

/// The canonical, sorted, deduped list of value-method names (across every
/// receiver type). Consumed by the shell's completer for `.`-position
/// (method/field) completion when the receiver's type is unknown; see
/// [`METHOD_NAMES`] and [`methods_for`].
pub fn method_names() -> &'static [&'static str] {
    &METHOD_NAMES
}

/// The single source of truth mapping a `Value::type_name` string to the
/// per-receiver method table `dispatch` accepts for it. Both the did-you-mean
/// hints ([`known_methods`]) and the receiver-aware completion set
/// ([`methods_for`]) read from *this* one match, and [`METHOD_NAMES`] is the
/// union of the very same `*_METHODS` constants — so a new dispatch arm added to
/// one table can't leave the others stale relative to each other.
///
/// `None` = the type has no dedicated per-receiver table because its method
/// surface is dynamic or forwarded (`stream` combinators, an `outcome`
/// forwarding to its `.out`, closures/commands/secrets/errors) — callers that
/// want a receiver-narrowed set should fall back to the full [`method_names`]
/// union rather than narrow to an inaccurate handful.
fn type_table(type_name: &str) -> Option<&'static [&'static str]> {
    Some(match type_name {
        "list" | "table" | "range" => SEQ_METHODS,
        "str" => STR_METHODS,
        "record" => RECORD_METHODS,
        "int" | "float" => NUM_METHODS,
        "path" => PATH_METHODS,
        "task" => TASK_METHODS,
        "bytes" => BYTES_METHODS,
        "bool" | "size" | "duration" | "datetime" | "time" => SCALAR_METHODS,
        _ => return None,
    })
}

fn known_methods(type_name: &str) -> &'static [&'static str] {
    type_table(type_name).unwrap_or(&[])
}

/// The method names applicable to a receiver of `type_name` (the string
/// [`Value::type_name`](crate::Value::type_name) returns), for receiver-aware
/// `.`-position completion in the shell. The universal
/// [`POLY_METHODS`] (`tap`/`also`, dispatched for every receiver) are folded in
/// so a type-narrowed set never drops the methods that apply to *all* types.
/// Sorted and deduped.
///
/// Returns `None` for a type with no dedicated per-receiver table (see
/// [`type_table`]); the caller should offer the full [`method_names`] union in
/// that case so it never presents *fewer* candidates than the type-agnostic
/// path would. Because the underlying tables are the same ones [`METHOD_NAMES`]
/// and the did-you-mean hints consume, `methods_for("list")` is always a subset
/// of `method_names()` and can't drift from what `dispatch` actually accepts.
pub fn methods_for(type_name: &str) -> Option<Vec<&'static str>> {
    let base = type_table(type_name)?;
    let mut v: Vec<&'static str> = base
        .iter()
        .copied()
        .chain(POLY_METHODS.iter().copied())
        .collect();
    v.sort_unstable();
    v.dedup();
    Some(v)
}

/// The `field_missing` error for an unknown method, with a did-you-mean hint
/// attached when a plausible near-miss exists.
pub(crate) fn unknown_method(name: &str, recv: &Value) -> ErrorVal {
    let err = ErrorVal::new(
        "field_missing",
        format!("unknown method `.{name}` on {}", recv.type_name()),
    );
    match hint(name, recv.type_name()) {
        Some(h) => err.with_hint(h),
        None => err,
    }
}

fn hint(name: &str, type_name: &str) -> Option<String> {
    let known = known_methods(type_name);
    // Curated alias-isms first — semantically right but too far apart for edit
    // distance to catch.
    match name {
        "length" | "size" if known.contains(&"len") => {
            return Some("did you mean .len()?".into());
        }
        "push" if matches!(type_name, "list" | "table") => {
            return Some(
                "shoal values are immutable — build a new list with `list + [item]`".into(),
            );
        }
        "substring" | "substr" | "slice" if type_name == "str" => {
            return Some("did you mean .take(n) / .skip(n)? they slice a str by char".into());
        }
        _ => {}
    }
    // `to_x` → `x` (`.to_upper` → `.upper`, `.to_str` → `.str`, …).
    if let Some(rest) = name.strip_prefix("to_")
        && known.contains(&rest)
    {
        return Some(format!("did you mean .{rest}()?"));
    }
    // Nearest known name by edit distance (strictly less than the name's own
    // length so short names can't match nonsense). Short names only tolerate
    // distance 1 — at distance 2 a 4-char name like `size` matches half the
    // table (`save`) — while names of ≥ 5 chars tolerate 2.
    let len = name.chars().count();
    let max_d = if len >= 5 { 2 } else { 1 };
    let best = known
        .iter()
        .map(|k| (levenshtein(name, k), *k))
        .min_by_key(|(d, _)| *d)?;
    if best.0 <= max_d && best.0 < len {
        return Some(format!("did you mean .{}()?", best.1));
    }
    // Suffix-variant alias-ism: `read_str` → `read`.
    known
        .iter()
        .find(|k| {
            k.len() >= 3
                && name.len() > k.len()
                && name.starts_with(**k)
                && name.as_bytes()[k.len()] == b'_'
        })
        .map(|k| format!("did you mean .{k}()?"))
}

/// Classic two-row Levenshtein edit distance (method names are short ASCII;
/// no dependency warranted). Exposed via `methods` so the evaluator's command
/// did-you-mean (TDD §13.9) reuses the very same metric the method did-you-mean
/// uses here, rather than duplicating it.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names_union_is_sorted_deduped_and_covers_receivers() {
        let names = method_names();
        // Representative methods from each receiver family are all present.
        for m in [
            "where", "map", "sort", "upper", "lower", "split", "keys", "values", "abs", "round",
            "name", "stem", "await", "cancel", "tap", "also",
        ] {
            assert!(names.contains(&m), "method_names() is missing `{m}`");
        }
        let mut want = names.to_vec();
        want.sort_unstable();
        want.dedup();
        assert_eq!(names, want.as_slice(), "must be sorted and deduped");
    }

    #[test]
    fn methods_for_is_a_subset_of_the_union_and_carries_poly_methods() {
        for ty in [
            "list", "str", "record", "int", "float", "path", "bytes", "task", "size",
        ] {
            let per_type = methods_for(ty).unwrap_or_else(|| panic!("{ty} should have a table"));
            // Every receiver-narrowed name is also in the flat union — the two
            // are built from the same `*_METHODS` constants, so this can only
            // hold if they haven't drifted apart.
            for m in &per_type {
                assert!(
                    method_names().contains(m),
                    "methods_for({ty}) offers `{m}` which is absent from method_names()"
                );
            }
            // The universal `tap`/`also` are folded into every concrete table.
            assert!(
                per_type.contains(&"tap"),
                "methods_for({ty}) must include tap"
            );
            assert!(
                per_type.contains(&"also"),
                "methods_for({ty}) must include also"
            );
            // Sorted + deduped.
            let mut want = per_type.clone();
            want.sort_unstable();
            want.dedup();
            assert_eq!(
                per_type, want,
                "methods_for({ty}) must be sorted and deduped"
            );
        }
        // A type with a dynamic/forwarded surface (or a nonexistent type) has no
        // dedicated table — callers fall back to the union.
        assert_eq!(methods_for("stream"), None);
        assert_eq!(methods_for("outcome"), None);
        assert_eq!(methods_for("closure"), None);
        assert_eq!(methods_for("bogus"), None);
    }

    #[test]
    fn methods_for_narrows_correctly_per_receiver() {
        // A list offers collection ops, never string-only ops.
        let list = methods_for("list").unwrap();
        for m in ["where", "map", "sum", "sort_by", "first"] {
            assert!(list.contains(&m), "list should offer `{m}`");
        }
        for m in ["upper", "lower", "split", "keys", "values"] {
            assert!(!list.contains(&m), "list must NOT offer `{m}`");
        }
        // A str offers string ops, not record/collection higher-order ops.
        let s = methods_for("str").unwrap();
        for m in ["upper", "len", "split", "trim", "starts_with"] {
            assert!(s.contains(&m), "str should offer `{m}`");
        }
        for m in ["where", "map", "keys"] {
            assert!(!s.contains(&m), "str must NOT offer `{m}`");
        }
        // A record offers record ops.
        let r = methods_for("record").unwrap();
        for m in ["keys", "values", "items", "merge", "get"] {
            assert!(r.contains(&m), "record should offer `{m}`");
        }
        for m in ["upper", "map", "where"] {
            assert!(!r.contains(&m), "record must NOT offer `{m}`");
        }
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("sort", "sort"), 0);
        assert_eq!(levenshtein("sortt", "sort"), 1);
    }

    #[test]
    fn alias_isms_and_typos() {
        assert_eq!(hint("length", "list").unwrap(), "did you mean .len()?");
        assert_eq!(hint("size", "str").unwrap(), "did you mean .len()?");
        assert_eq!(hint("to_upper", "str").unwrap(), "did you mean .upper()?");
        assert_eq!(hint("to_lower", "str").unwrap(), "did you mean .lower()?");
        assert_eq!(hint("read_str", "path").unwrap(), "did you mean .read()?");
        assert_eq!(hint("sortt", "list").unwrap(), "did you mean .sort()?");
        assert!(hint("substring", "str").unwrap().contains(".take"));
        assert!(hint("push", "list").unwrap().contains("immutable"));
        // `.size` on int has no `.len` — no bogus suggestion.
        assert_eq!(hint("size", "int"), None);
        // Totally unrelated names stay hint-free.
        assert_eq!(hint("frobnicate", "list"), None);
    }
}
