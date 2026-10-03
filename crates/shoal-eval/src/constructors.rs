//! Canonical metadata for evaluator-owned value constructors.
//!
//! Runtime dispatch and static effect derivation both consume this enum so a
//! constructor can never silently become an external command in one surface.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Constructor {
    Path,
    Glob,
    Regex,
    Channel,
    Every,
    Watch,
    Tail,
}

impl Constructor {
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "path" => Self::Path,
            "glob" => Self::Glob,
            "regex" => Self::Regex,
            "channel" => Self::Channel,
            "every" => Self::Every,
            "watch" => Self::Watch,
            "tail" => Self::Tail,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_inventory_is_explicit() {
        for name in ["path", "glob", "regex", "channel", "every", "watch", "tail"] {
            assert!(Constructor::named(name).is_some(), "missing {name}");
        }
        assert_eq!(Constructor::named("not-a-constructor"), None);
    }
}
