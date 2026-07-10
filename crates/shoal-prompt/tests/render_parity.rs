//! design §2.1's pinned test obligation: shoal-prompt's duration/size formatters
//! must agree byte-for-byte with `shoal_value::render::render_inline` over a
//! fixed corpus. This is the tripwire that fires if the intentional formatter
//! duplication ever drifts — not a user "the duration looks wrong" bug.
//!
//! shoal-value is a DEV-dependency only here (never a normal dependency); this
//! test cannot ship in the compiled binary's dependency graph.

use shoal_prompt::{format_duration_ns, format_size};
use shoal_value::Value;
use shoal_value::render::render_inline;

#[test]
fn duration_formatter_matches_shoal_value() {
    // 0ns, 250ms, 1s, 1s500ms, 1m30s, 3h, 1d2h
    let corpus: [i64; 7] = [
        0,
        250_000_000,
        1_000_000_000,
        1_500_000_000,
        90_000_000_000,
        3 * 3_600_000_000_000,
        86_400_000_000_000 + 2 * 3_600_000_000_000,
    ];
    for ns in corpus {
        assert_eq!(
            format_duration_ns(ns),
            render_inline(&Value::Duration(ns)),
            "duration parity mismatch at {ns} ns"
        );
    }
}

#[test]
fn size_formatter_matches_shoal_value() {
    // 0b, 237b, 1500b, 1_500_000b, 1_020_000_000b
    let corpus: [u64; 5] = [0, 237, 1500, 1_500_000, 1_020_000_000];
    for bytes in corpus {
        assert_eq!(
            format_size(bytes),
            render_inline(&Value::Size(bytes)),
            "size parity mismatch at {bytes} bytes"
        );
    }
}
