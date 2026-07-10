//! Theme presets, shipped in the binary via `include_str!` — no filesystem
//! dependency, no install step (§3.7). A theme is *not special* beyond being an
//! extra lowest-precedence config layer (see [`crate::config::load`]).

const DEFAULT: &str = include_str!("../themes/default.toml");
const RICH: &str = include_str!("../themes/rich.toml");
const MINIMAL: &str = include_str!("../themes/minimal.toml");

/// The shipped theme names.
pub const NAMES: &[&str] = &["default", "rich", "minimal"];

/// Fetch a shipped theme's raw TOML by name (§3.7). Returns `None` for an
/// unknown name (the caller emits a "unknown theme" warning).
pub fn get(name: &str) -> Option<&'static str> {
    match name {
        "default" => Some(DEFAULT),
        "rich" => Some(RICH),
        "minimal" => Some(MINIMAL),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_theme_parses_and_loads() {
        for name in NAMES {
            let src = get(name).expect("shipped theme resolves");
            let val: toml::Value = toml::from_str(src).expect("theme is valid TOML");
            let mut warnings = Vec::new();
            crate::config::load(vec![val], &mut warnings);
            // Themes must not trip the unknown-key validator.
            assert!(
                warnings.is_empty(),
                "theme '{name}' produced warnings: {warnings:?}"
            );
        }
    }

    #[test]
    fn unknown_theme_is_none() {
        assert!(get("nope").is_none());
    }
}
