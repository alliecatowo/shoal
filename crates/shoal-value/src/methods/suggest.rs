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

fn known_methods(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "list" | "table" | "range" => SEQ_METHODS,
        "str" => STR_METHODS,
        "record" => RECORD_METHODS,
        "int" | "float" => NUM_METHODS,
        "path" => PATH_METHODS,
        "task" => TASK_METHODS,
        "bytes" => BYTES_METHODS,
        "bool" | "size" | "duration" | "datetime" | "time" => SCALAR_METHODS,
        _ => &[],
    }
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
/// no dependency warranted).
fn levenshtein(a: &str, b: &str) -> usize {
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
