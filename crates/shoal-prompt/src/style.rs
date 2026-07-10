//! Style-spec grammar (§3.6) and a tiny in-crate ANSI SGR styler.
//!
//! design-prompt.md §2.1: the style spec is ~40 lines of `match`, not worth a
//! crate edge for a domain-pure library, so shoal-prompt emits plain ANSI SGR
//! sequences directly rather than depending on `nu-ansi-term`.

/// A parsed style: foreground/background colors plus attribute flags. `none`
/// records an explicit opt-out that suppresses even a theme's default style.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dimmed: bool,
    /// Explicit `none` token seen — style is a deliberate no-op.
    pub none: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Color {
    Named(&'static str),
    Ansi256(u8),
    Rgb(u8, u8, u8),
}

impl Style {
    /// True when this style would emit no SGR at all (empty spec or `none`).
    pub fn is_plain(&self) -> bool {
        self.none
            || (self.fg.is_none()
                && self.bg.is_none()
                && !self.bold
                && !self.italic
                && !self.underline
                && !self.dimmed)
    }

    /// Wrap `text` in this style's SGR sequence, resetting after. Returns the
    /// text unchanged when `no_color` is set (the no-color.org contract: NO
    /// color, checked once, no per-module opt-out) or when the style is plain.
    pub fn paint(&self, text: &str, no_color: bool) -> String {
        if no_color || self.is_plain() || text.is_empty() {
            return text.to_string();
        }
        let mut codes: Vec<String> = Vec::new();
        if self.bold {
            codes.push("1".into());
        }
        if self.dimmed {
            codes.push("2".into());
        }
        if self.italic {
            codes.push("3".into());
        }
        if self.underline {
            codes.push("4".into());
        }
        if let Some(c) = &self.fg {
            codes.push(c.sgr(false));
        }
        if let Some(c) = &self.bg {
            codes.push(c.sgr(true));
        }
        if codes.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

impl Color {
    fn sgr(&self, background: bool) -> String {
        let base = if background { 48 } else { 38 };
        match self {
            Color::Named(name) => {
                let code = named_base(name);
                // 30-37 fg / 40-47 bg for standard, 90-97 / 100-107 for bright.
                if code >= 8 {
                    let n = code - 8;
                    if background {
                        (100 + n).to_string()
                    } else {
                        (90 + n).to_string()
                    }
                } else if background {
                    (40 + code).to_string()
                } else {
                    (30 + code).to_string()
                }
            }
            Color::Ansi256(n) => format!("{base};5;{n}"),
            Color::Rgb(r, g, b) => format!("{base};2;{r};{g};{b}"),
        }
    }
}

fn named_base(name: &str) -> u8 {
    match name {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "purple" | "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        "bright-black" => 8,
        "bright-red" => 9,
        "bright-green" => 10,
        "bright-yellow" => 11,
        "bright-blue" => 12,
        "bright-purple" | "bright-magenta" => 13,
        "bright-cyan" => 14,
        "bright-white" => 15,
        _ => 0,
    }
}

const NAMED: &[&str] = &[
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "purple",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-purple",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
];

/// Parse a space-separated style spec into a [`Style`]. Unknown tokens are
/// ignored; `warnings` collects a note when the spec yields zero recognized
/// tokens (a typo like `"boldd"`, §11) — never an error, matching the project's
/// warn-don't-crash posture. Tokens are order-independent, last-color-wins.
pub fn parse_style(spec: &str, warnings: &mut Vec<String>) -> Style {
    let mut style = Style::default();
    let mut recognized = 0usize;
    for tok in spec.split_whitespace() {
        recognized += 1;
        match tok {
            "none" => style.none = true,
            "bold" => style.bold = true,
            "italic" => style.italic = true,
            "underline" => style.underline = true,
            "dim" | "dimmed" => style.dimmed = true,
            _ if tok.starts_with("bg:") => {
                if let Some(c) = parse_color(&tok[3..]) {
                    style.bg = Some(c);
                } else {
                    recognized -= 1;
                }
            }
            _ => {
                if let Some(c) = parse_color(tok) {
                    style.fg = Some(c);
                } else {
                    recognized -= 1;
                }
            }
        }
    }
    if recognized == 0 && !spec.trim().is_empty() {
        warnings.push(format!(
            "prompt: style spec '{spec}' has no recognized tokens"
        ));
    }
    style
}

fn parse_color(tok: &str) -> Option<Color> {
    if let Some(hex) = tok.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    if let Ok(n) = tok.parse::<u16>() {
        if n <= 255 {
            return Some(Color::Ansi256(n as u8));
        }
        return None;
    }
    NAMED.iter().find(|n| **n == tok).map(|n| Color::Named(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(spec: &str) -> Style {
        parse_style(spec, &mut Vec::new())
    }

    #[test]
    fn parses_named_and_attrs() {
        let st = s("green bold");
        assert_eq!(st.fg, Some(Color::Named("green")));
        assert!(st.bold);
    }

    #[test]
    fn ansi256_and_hex_and_bg() {
        assert_eq!(s("8").fg, Some(Color::Ansi256(8)));
        assert_eq!(s("#ff8800").fg, Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert_eq!(s("bg:red").bg, Some(Color::Named("red")));
    }

    #[test]
    fn none_is_plain_and_paints_nothing() {
        let st = s("none");
        assert!(st.none);
        assert_eq!(st.paint("x", false), "x");
    }

    #[test]
    fn no_color_suppresses_sgr() {
        let st = s("red bold");
        assert_eq!(st.paint("x", true), "x");
        assert!(st.paint("x", false).contains("\x1b["));
    }

    #[test]
    fn typo_warns_and_falls_back_to_plain() {
        let mut w = Vec::new();
        let st = parse_style("boldd", &mut w);
        assert!(st.is_plain());
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn bright_and_bg_sgr_codes() {
        assert_eq!(Color::Named("bright-red").sgr(false), "91");
        assert_eq!(Color::Named("red").sgr(true), "41");
        assert_eq!(Color::Ansi256(200).sgr(false), "38;5;200");
    }
}
