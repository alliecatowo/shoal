//! Duration/size formatting — an intentional, parity-tested duplicate of the
//! rules pinned in CONTRACTS.md §3 (`shoal_value::render::render_inline`).
//!
//! design-prompt.md §2.1 makes this the one deliberate seam-crossing risk and
//! closes it with `tests/render_parity.rs`, which asserts these functions agree
//! with `shoal-value` byte-for-byte over a fixed corpus. If the two ever drift,
//! that test is the tripwire — not a user bug report.

use std::time::Duration;

/// Compound duration: `1m30s`, `250ms`, `0s`, `1s500ms`. Mirrors
/// `shoal_value::render::render_duration` exactly.
pub fn format_duration(d: Duration) -> String {
    // shoal-value works in i64 nanoseconds; saturate rather than panic on the
    // (practically impossible) overflow of a >292-year Duration.
    let ns = i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
    format_duration_ns(ns)
}

/// The same algorithm keyed on raw signed nanoseconds (negative → `-` prefix).
pub fn format_duration_ns(ns: i64) -> String {
    if ns == 0 {
        return "0s".to_string();
    }
    let (sign, mut rest) = if ns < 0 {
        ("-", ns.unsigned_abs())
    } else {
        ("", ns.unsigned_abs())
    };
    const UNITS: [(&str, u64); 8] = [
        ("w", 604_800_000_000_000),
        ("d", 86_400_000_000_000),
        ("h", 3_600_000_000_000),
        ("m", 60_000_000_000),
        ("s", 1_000_000_000),
        ("ms", 1_000_000),
        ("us", 1_000),
        ("ns", 1),
    ];
    let mut out = String::from(sign);
    for (name, mult) in UNITS {
        if rest >= mult {
            let n = rest / mult;
            rest %= mult;
            out.push_str(&format!("{n}{name}"));
        }
    }
    out
}

/// Humanized decimal size: `237b`, `1.5mb`, `1.02kb`. Mirrors
/// `shoal_value::render::render_size` exactly.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [("tb", 1e12), ("gb", 1e9), ("mb", 1e6), ("kb", 1e3)];
    if bytes < 1000 {
        return format!("{bytes}b");
    }
    for (i, (name, mult)) in UNITS.iter().enumerate() {
        if (bytes as f64) >= *mult {
            let mut x = bytes as f64 / mult;
            let mut name = *name;
            if format!("{x:.2}").parse::<f64>().unwrap_or(x) >= 1000.0 && i > 0 {
                name = UNITS[i - 1].0;
                x = bytes as f64 / UNITS[i - 1].1;
            }
            let mut s = format!("{x:.2}");
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
            return format!("{s}{name}");
        }
    }
    unreachable!("bytes >= 1000 always matches a unit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(format_duration_ns(0), "0s");
        assert_eq!(format_duration_ns(250_000_000), "250ms");
        assert_eq!(format_duration_ns(90_000_000_000), "1m30s");
        assert_eq!(format_duration_ns(1_500_000_000), "1s500ms");
        assert_eq!(format_duration(Duration::from_secs(1)), "1s");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0b");
        assert_eq!(format_size(237), "237b");
        assert_eq!(format_size(1_500_000), "1.5mb");
        assert_eq!(format_size(999_999), "1mb");
    }
}
