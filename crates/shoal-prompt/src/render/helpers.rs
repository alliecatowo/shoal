//! Free helper functions used by module rendering (site/content/internals/prompt-editor-lsp.md): path/branch
//! truncation, home-dir collapsing, version trimming, and the tiny
//! strftime subset used by `$time`.

use crate::context::PromptContext;

pub(super) fn collapse_home(ctx: &PromptContext, home_symbol: &str) -> String {
    if let Some(home) = &ctx.home
        && let Ok(tail) = ctx.cwd.strip_prefix(home)
    {
        if tail.as_os_str().is_empty() {
            return home_symbol.to_string();
        }
        return format!("{home_symbol}/{}", tail.to_string_lossy());
    }
    ctx.cwd.to_string_lossy().into_owned()
}

/// Keep the last `n` path segments, prefixing an ellipsis when more existed.
pub(super) fn truncate_path(path: &str, n: usize, ellipsis: &str) -> String {
    let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let leading_slash = path.starts_with('/');
    if comps.len() <= n {
        return path.to_string();
    }
    let kept = &comps[comps.len() - n..];
    // Truncated: the ellipsis stands in for the elided prefix. `leading_slash`
    // is intentionally dropped here — a truncated absolute path shows the
    // ellipsis, not the original root.
    let _ = leading_slash;
    format!("{ellipsis}/{}", kept.join("/"))
}

pub(super) fn truncate_branch(branch: &str, n: usize, symbol: &str) -> String {
    if n == 0 {
        return branch.to_string();
    }
    let chars: Vec<char> = branch.chars().collect();
    if chars.len() <= n {
        return branch.to_string();
    }
    let head: String = chars[..n].iter().collect();
    format!("{head}{symbol}")
}

/// Drop the patch component of a semver-ish version (site/content/internals/prompt-editor-lsp.md: "no patch").
pub(super) fn short_version(v: &str) -> String {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() >= 3 {
        parts[..2].join(".")
    } else {
        v.to_string()
    }
}

/// A tiny strftime subset over `(hour, min, sec)` only (site/content/internals/prompt-editor-lsp.md — no date in v1).
pub(super) fn strftime_hms(fmt: &str, h: u8, m: u8, s: u8) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('H') => out.push_str(&format!("{h:02}")),
                Some('M') => out.push_str(&format!("{m:02}")),
                Some('S') => out.push_str(&format!("{s:02}")),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_path_keeps_last_n() {
        assert_eq!(truncate_path("~/a/b/c/d", 2, "…"), "…/c/d");
        assert_eq!(truncate_path("~/a", 3, "…"), "~/a");
    }

    #[test]
    fn short_version_drops_patch() {
        assert_eq!(short_version("3.12.1"), "3.12");
        assert_eq!(short_version("22"), "22");
    }

    #[test]
    fn strftime_subset() {
        assert_eq!(strftime_hms("%H:%M:%S", 9, 5, 30), "09:05:30");
        assert_eq!(strftime_hms("%H%%", 12, 0, 0), "12%");
    }
}
