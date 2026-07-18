//! Supporting scalar/handle payloads — `GlobVal`, `RegexVal`, `TimeVal`,
//! `RangeVal`, `ClosureVal`, `SecretVal` — plus the bind-time word-parsing
//! helpers (`parse_size`/`parse_duration`/`parse_time`; see
//! `site/content/internals/language-conformance-contract.md`), moved
//! verbatim out of `lib.rs`.

use super::*;
use crate::ports::BytesLoad;

/// A lazy, content-addressed bytes value (site/content/internals/language-conformance-contract.md disk-spill). Produced when a
/// value-position capture's stdout overflowed the RAM cap and was spilled to the
/// CAS: the full bytes live on disk under [`hash`](CasBytesVal::hash), only a
/// bounded [`preview`](CasBytesVal::preview) is resident, and the full content
/// is loaded from the CAS on demand via [`loader`](CasBytesVal::loader).
///
/// `.len` and `render` answer from the metadata alone (never loading); methods
/// that need the whole bytes materialize them through [`CasBytesVal::resolve`].
/// A small (sub-cap) capture is a plain [`Value::Bytes`] and never becomes one
/// of these — there is zero change to the common, fully-resident path.
///
/// # Central CAS-bytes chokepoint
///
/// Before this refactor, awareness of this variant was scattered across ~6
/// call sites, each hand-rolling its own little match on `name`/metadata. Two
/// methods here are the centralized answer:
///
/// - [`cheap_method`](Self::cheap_method) — the ONE `name -> metadata-only
///   answer` table (`len`/`count`/`is_empty`/`ref`), called from
///   `methods::dispatch`'s CasBytes arm instead of a duplicated inline match.
/// - [`json_preview`](Self::json_preview) — the deliberate, bounded answer for
///   a CasBytes value found NESTED inside a larger value being JSON-encoded
///   (a record/table field, a `.feed`'d structure, a direct
///   `json.stringify`/`yaml.stringify`/`toml.stringify` argument). Before this
///   change, `value_to_json` (`crate::json`) called [`resolve`](Self::resolve)
///   unconditionally there — a value the caller never asked to see in full
///   could silently pull up to the full spill cap (~1 GiB) into memory just
///   because it happened to be a field of something being serialized. That
///   was the main risk this audit flagged; `json_preview` fixes it by
///   answering from the resident preview + metadata only, exactly like
///   `render()` does, never touching the CAS. A bare top-level `.json()` on
///   the bytes themselves is unaffected: `methods::dispatch`'s CasBytes
///   fallback already fully materializes (and converts to `Value::Bytes`)
///   *before* `value_to_json` is ever reached, so that call site keeps its
///   existing, deliberate full-load behavior.
///
/// Everywhere else this type's awareness turned out to already be correct and
/// singular, not scattered: `ops.rs`'s arithmetic/comparison/`contains` tables
/// have no `Bytes` arm either (both hit the same generic type-error fallback,
/// consistently); structural equality (`lib.rs`) and `render.rs` each read
/// straight off the metadata fields with no duplicated match to centralize —
/// their existing behavior (a spilled value comparing unequal to a
/// byte-identical resident one; a ref-carrying render distinct from a bare
/// `Bytes` render) is a documented wrinkle, preserved here on purpose rather
/// than "fixed", since silently reinterpreting it would be an observable
/// behavior change nobody asked for.
pub struct CasBytesVal {
    /// blake3 hex of the full stored content — the recoverable `val:blake3:…`
    /// ref (site/content/internals/kernel-protocol.md elision doctrine, in-language).
    pub hash: String,
    /// True total length of the content in bytes (what `.len` returns).
    pub len: u64,
    /// Bounded resident prefix, for cheap `render` previews and small ops.
    pub preview: Arc<Vec<u8>>,
    /// `true` when even the on-disk spill was itself capped (the stored bytes,
    /// and thus this value, are a prefix of what the command actually produced).
    pub truncated: bool,
    /// Loads the full content from the CAS on demand (see [`BytesLoad`]).
    pub loader: Arc<dyn BytesLoad>,
}

impl CasBytesVal {
    /// The scheme prefix of a recoverable content short-ref: the wire/CAS form
    /// `.ref` yields and the in-language dispatch path recognizes (site/content/internals/language-conformance-contract.md).
    pub const REF_PREFIX: &'static str = "val:blake3:";

    /// Parse a `val:blake3:<hash>` content short-ref, returning the bare blake3
    /// hex on a match. `None` when `s` is not a content ref of this scheme — the
    /// single place the ref grammar is decoded, mirroring [`Self::reference`]
    /// (which encodes it), so the wire form and the in-language resolver agree.
    pub fn parse_ref(s: &str) -> Option<&str> {
        s.strip_prefix(Self::REF_PREFIX)
    }

    /// Load the full content from the CAS, mapping any I/O/integrity failure to
    /// an `io_error` [`ErrorVal`].
    pub fn resolve(&self) -> VResult<Vec<u8>> {
        self.loader.load().map_err(|e| {
            ErrorVal::new(
                "io_error",
                format!("failed to load CAS-backed bytes {}: {e}", self.reference()),
            )
        })
    }

    /// Open the full content for bounded-memory sequential consumption.
    pub fn open(&self) -> VResult<Box<dyn std::io::Read + Send>> {
        self.loader.open().map_err(|e| {
            ErrorVal::new(
                "io_error",
                format!("failed to open CAS-backed bytes {}: {e}", self.reference()),
            )
        })
    }

    /// The recoverable content ref, e.g. `val:blake3:<hash>`.
    pub fn reference(&self) -> String {
        format!("{}{}", Self::REF_PREFIX, self.hash)
    }

    /// The metadata-only answer for a method name, when one exists — never
    /// loads from the CAS. The single chokepoint for the cheap-answer table
    /// (`len`/`count`/`is_empty`/`ref`); `None` means the caller needs the
    /// actual bytes (`methods::dispatch` then either loads explicitly for
    /// `load`/`bytes`, or fully materializes and re-dispatches as a plain
    /// `Value::Bytes` for anything else).
    pub fn cheap_method(&self, name: &str) -> Option<Value> {
        match name {
            "len" | "count" => Some(Value::Int(self.len as i64)),
            "is_empty" => Some(Value::Bool(self.len == 0)),
            "ref" => Some(Value::Str(self.reference())),
            _ => None,
        }
    }

    /// The bounded, lazy JSON representation used when this value is
    /// encountered NESTED inside a larger value being JSON-encoded (see the
    /// struct doc's "Central CAS-bytes chokepoint" section for why this exists and why it's safe
    /// relative to the bare top-level `.json()` method, which fully
    /// materializes through a different, deliberate call site). Never loads
    /// from the CAS: just the recoverable ref, the true length, the
    /// truncation flag, and the already-resident preview — the same
    /// information `render()` shows, shaped as JSON instead of a display
    /// string.
    pub fn json_preview(&self) -> serde_json::Value {
        serde_json::json!({
            "$": "bytes_ref",
            "ref": self.reference(),
            "len": self.len,
            "truncated": self.truncated,
            "preview": String::from_utf8_lossy(&self.preview),
        })
    }
}

impl std::fmt::Debug for CasBytesVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CasBytesVal")
            .field("hash", &self.hash)
            .field("len", &self.len)
            .field("preview_len", &self.preview.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobVal {
    pub pattern: String,
    /// Origin cwd — expansion always happens against this (site/content/internals/language-conformance-contract.md).
    pub cwd: PathBuf,
    pub hidden: bool,
}

#[derive(Debug)]
pub struct RegexVal {
    pub src: String,
    pub re: regex::Regex,
}

impl RegexVal {
    pub fn compile(src: &str) -> VResult<RegexVal> {
        regex::Regex::new(src)
            .map(|re| RegexVal {
                src: src.to_string(),
                re,
            })
            .map_err(|e| ErrorVal::new("arg_error", format!("invalid regex: {e}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeVal {
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeVal {
    pub start: i64,
    pub end: i64,
    pub inclusive: bool,
}

/// Maximum number of scalar values a compact range may expand into at one
/// eager materialization boundary. Lazy iteration/streaming remains available
/// for larger ranges.
pub const RANGE_MATERIALIZATION_MAX_VALUES: usize = 16_384;

pub struct RangeIter {
    next: Option<i64>,
    end: i64,
    inclusive: bool,
}

impl Iterator for RangeIter {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.next?;
        let in_range = if self.inclusive {
            current <= self.end
        } else {
            current < self.end
        };
        if !in_range {
            self.next = None;
            return None;
        }
        self.next = current.checked_add(1);
        Some(current)
    }
}

impl RangeVal {
    pub fn iter(&self) -> RangeIter {
        RangeIter {
            next: Some(self.start),
            end: self.end,
            inclusive: self.inclusive,
        }
    }
    pub fn len(&self) -> u128 {
        let start = i128::from(self.start);
        let end = i128::from(self.end);
        if self.inclusive {
            if end < start {
                0
            } else {
                (end - start + 1) as u128
            }
        } else if end <= start {
            0
        } else {
            (end - start) as u128
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn materialization_len(&self) -> VResult<usize> {
        let len = self.len();
        if len > RANGE_MATERIALIZATION_MAX_VALUES as u128 {
            return Err(ErrorVal::new(
                "range_materialization_limit",
                format!(
                    "range has {len} values; eager materialization is limited to {RANGE_MATERIALIZATION_MAX_VALUES}"
                ),
            )
            .with_hint("iterate lazily with `.stream()`, then use `.take(n)` when a list is required"));
        }
        Ok(usize::try_from(len).expect("admitted range length fits usize"))
    }
    pub fn materialize(&self) -> VResult<Vec<Value>> {
        let len = self.materialization_len()?;
        let mut values = Vec::with_capacity(len);
        values.extend(self.iter().map(Value::Int));
        Ok(values)
    }
    pub fn value_at(&self, index: i64) -> Option<i64> {
        let start = i128::from(self.start);
        let end_exclusive = i128::from(self.end) + i128::from(self.inclusive);
        let candidate = if index >= 0 {
            start + i128::from(index)
        } else {
            end_exclusive + i128::from(index)
        };
        (candidate >= start && candidate < end_exclusive)
            .then(|| i64::try_from(candidate).ok())
            .flatten()
    }
    pub fn contains(&self, v: i64) -> bool {
        v >= self.start
            && (if self.inclusive {
                v <= self.end
            } else {
                v < self.end
            })
    }
}

// ---------------------------------------------------------------------------
// Closures
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ClosureVal {
    /// `None` for lambdas; `Some` for `fn` declarations (drives `--help`).
    pub name: Option<String>,
    pub params: Vec<ast::Param>,
    pub rest: Option<ast::RestParam>,
    pub ret: Option<ast::Type>,
    pub body: ast::Expr,
    pub env: Env,
    pub doc: Option<String>,
}

#[derive(Clone)]
pub struct SecretVal {
    pub name: String,
    /// The secret material; never rendered, never journaled.
    pub value: Arc<str>,
}
impl std::fmt::Debug for SecretVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("secret").field(&self.name).finish()
    }
}

// ---------------------------------------------------------------------------
// Word parsing helpers (bind-time coercion)
// ---------------------------------------------------------------------------

/// Parse a size word like `1.5gb`, `4kib`, `237b`. Decimal units and binary
/// (`*ib`) units per site/content/internals/language-conformance-contract.md.
pub fn parse_size(word: &str) -> Option<u64> {
    let lower = word.to_ascii_lowercase();
    let split = lower.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = lower.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    if num < 0.0 {
        return None;
    }
    Some((num * mult).round() as u64)
}

/// Parse a duration word like `250ms`, `1.5h`, `30d`, or compound `1m30s`.
pub fn parse_duration(word: &str) -> Option<i64> {
    let lower = word.to_ascii_lowercase();
    let (neg, rest) = match lower.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, lower.as_str()),
    };
    let mut total: f64 = 0.0;
    let mut cur = rest;
    let mut any = false;
    while !cur.is_empty() {
        let split = cur.find(|c: char| c.is_ascii_alphabetic())?;
        if split == 0 {
            return None;
        }
        let (num, tail) = cur.split_at(split);
        let unit_end = tail
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(tail.len());
        let (unit, next) = tail.split_at(unit_end);
        let num: f64 = num.parse().ok()?;
        let ns: f64 = match unit {
            "ns" => 1.0,
            "us" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3_600e9,
            "d" => 86_400e9,
            "w" => 604_800e9,
            _ => return None,
        };
        total += num * ns;
        cur = next;
        any = true;
    }
    if !any {
        return None;
    }
    let v = total.round() as i64;
    Some(if neg { -v } else { v })
}

/// Parse a time word like `10:00am`, `23:15`, `07:30:15`.
pub fn parse_time(word: &str) -> Option<TimeVal> {
    let lower = word.to_ascii_lowercase();
    let (body, meridiem) = if let Some(b) = lower.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (lower.as_str(), None)
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut hour: u8 = parts[0].parse().ok()?;
    let min: u8 = parts[1].parse().ok()?;
    let sec: u8 = if parts.len() == 3 {
        parts[2].parse().ok()?
    } else {
        0
    };
    match meridiem {
        Some(pm) => {
            if hour == 0 || hour > 12 {
                return None;
            }
            if pm && hour != 12 {
                hour += 12;
            }
            if !pm && hour == 12 {
                hour = 0;
            }
        }
        None => {
            if hour > 23 {
                return None;
            }
        }
    }
    if min > 59 || sec > 59 {
        return None;
    }
    Some(TimeVal { hour, min, sec })
}

/// Test-only support for the `CasBytesVal` chokepoint tests,
/// `pub(crate)` so other files' test modules (`json.rs`, `methods/mod.rs`)
/// can build a fake spilled value without a real journal/CAS, and assert
/// whether its loader was actually invoked.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{Arc, BytesLoad, CasBytesVal};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A [`BytesLoad`] that counts every call and hands back fixed content —
    /// the probe a test uses to prove a "cheap" path never loads while a
    /// materializing one loads exactly once.
    pub(crate) struct CountingLoader {
        pub(crate) calls: Arc<AtomicUsize>,
        pub(crate) data: Vec<u8>,
    }

    impl BytesLoad for CountingLoader {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.data.clone())
        }
    }

    /// Build a `CasBytesVal` with `preview` resident and `full` as the
    /// "on-disk" content, wired to a loader that increments `calls` on every
    /// actual load.
    pub(crate) fn cas_bytes(preview: &[u8], full: &[u8], calls: Arc<AtomicUsize>) -> CasBytesVal {
        CasBytesVal {
            hash: "deadbeefcafef00d".into(),
            len: full.len() as u64,
            preview: Arc::new(preview.to_vec()),
            truncated: preview.len() < full.len(),
            loader: Arc::new(CountingLoader {
                calls,
                data: full.to_vec(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn ranges_are_exact_at_integer_edges_and_bound_eager_expansion() {
        let empty = RangeVal {
            start: i64::MIN,
            end: i64::MIN,
            inclusive: false,
        };
        assert!(empty.is_empty());
        assert_eq!(empty.iter().next(), None);
        assert_eq!(empty.value_at(0), None);
        assert_eq!(empty.value_at(-1), None);

        let full = RangeVal {
            start: i64::MIN,
            end: i64::MAX,
            inclusive: true,
        };
        assert_eq!(full.len(), u64::MAX as u128 + 1);
        assert_eq!(
            full.iter().take(2).collect::<Vec<_>>(),
            [i64::MIN, i64::MIN + 1]
        );
        assert_eq!(full.value_at(0), Some(i64::MIN));
        assert_eq!(full.value_at(-1), Some(i64::MAX));
        assert_eq!(
            full.materialize().unwrap_err().code,
            "range_materialization_limit"
        );
    }

    #[test]
    fn content_ref_encode_decode_roundtrip() {
        // `parse_ref` strips exactly the prefix `reference()` writes.
        assert_eq!(CasBytesVal::REF_PREFIX, "val:blake3:");
        assert_eq!(
            CasBytesVal::parse_ref("val:blake3:deadbeef"),
            Some("deadbeef")
        );
        assert_eq!(
            CasBytesVal::parse_ref(&format!("{}cafef00d", CasBytesVal::REF_PREFIX)),
            Some("cafef00d")
        );
        // Non-refs (a transcript short-ref, a bare algorithm tag, plain text)
        // are left alone so ordinary strings keep dispatching string methods.
        assert_eq!(CasBytesVal::parse_ref("out:5"), None);
        assert_eq!(CasBytesVal::parse_ref("blake3:deadbeef"), None);
        assert_eq!(CasBytesVal::parse_ref("val:blake2:deadbeef"), None);
        assert_eq!(CasBytesVal::parse_ref("hello world"), None);
    }

    /// `cheap_method` answers `len`/`count`/`is_empty`/`ref` from metadata
    /// alone, never touching the loader.
    #[test]
    fn cheap_method_never_loads() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"hel", b"hello world", calls.clone());
        assert_eq!(c.cheap_method("len"), Some(Value::Int(11)));
        assert_eq!(c.cheap_method("count"), Some(Value::Int(11)));
        assert_eq!(c.cheap_method("is_empty"), Some(Value::Bool(false)));
        assert_eq!(
            c.cheap_method("ref"),
            Some(Value::Str("val:blake3:deadbeefcafef00d".into()))
        );
        // A name outside the cheap table defers to the caller.
        assert_eq!(c.cheap_method("upper"), None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "cheap_method must not load"
        );
    }

    #[test]
    fn empty_cas_bytes_is_empty() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"", b"", calls.clone());
        assert_eq!(c.cheap_method("is_empty"), Some(Value::Bool(true)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// `json_preview` answers from the ref/length/
    /// truncation/preview metadata alone — never loads.
    #[test]
    fn json_preview_never_loads() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"hel", b"hello world", calls.clone());
        let j = c.json_preview();
        assert_eq!(j["$"], "bytes_ref");
        assert_eq!(j["ref"], "val:blake3:deadbeefcafef00d");
        assert_eq!(j["len"], 11);
        assert_eq!(j["truncated"], true);
        assert_eq!(j["preview"], "hel");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "json_preview must not load"
        );
    }

    /// `resolve()` — the deliberate full-materialize chokepoint used by the
    /// bare top-level `.json()`/`.load`/`.bytes`/`.feed` call sites — loads
    /// exactly once and returns the true full content, not just the preview.
    #[test]
    fn resolve_loads_full_content_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"hel", b"hello world", calls.clone());
        assert_eq!(c.resolve().unwrap(), b"hello world".to_vec());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
