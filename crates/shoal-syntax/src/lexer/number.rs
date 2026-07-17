//! Numeric literal scanning: ints (with radix prefixes), floats, size/duration
//! unit suffixes, and time-of-day literals (`10:00am`, `23:15`).

use super::*;

impl<'s> Lexer<'s> {
    pub(crate) fn number(&self, start: usize) -> LexResult {
        let mut pos = start;
        // Radix prefixes. A prefix only applies when a valid digit of that
        // radix actually follows — otherwise `0b` is the size literal `0` +
        // unit `b` (zero bytes), and `0x`/`0o` likewise fall through to the
        // decimal/size path rather than erroring on missing digits.
        let radix_digit_follows = |b: u8, radix: u32| -> bool { (b as char).is_digit(radix) };
        if self.at(pos) == b'0'
            && matches!(self.at(pos + 1), b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
            && {
                let radix = match self.at(pos + 1) {
                    b'x' | b'X' => 16,
                    b'o' | b'O' => 8,
                    _ => 2,
                };
                radix_digit_follows(self.at(pos + 2), radix)
            }
        {
            let radix = match self.at(pos + 1) {
                b'x' | b'X' => 16,
                b'o' | b'O' => 8,
                _ => 2,
            };
            pos += 2;
            let digits_start = pos;
            while pos < self.bytes.len()
                && (self.at(pos).is_ascii_alphanumeric() || self.at(pos) == b'_')
            {
                pos += 1;
            }
            let digits: String = self.src[digits_start..pos]
                .chars()
                .filter(|&c| c != '_')
                .collect();
            let v = i64::from_str_radix(&digits, radix)
                .map_err(|_| LexError::new("invalid numeric literal", Span::new(start, pos)))?;
            return Ok((Tok::Int(v), Span::new(start, pos)));
        }

        let mut is_float = false;
        while pos < self.bytes.len() && (self.at(pos).is_ascii_digit() || self.at(pos) == b'_') {
            pos += 1;
        }
        // Time literal: \d{1,2}:\d{2}(:\d{2})?(am|pm)?
        if self.at(pos) == b':' && self.at(pos + 1).is_ascii_digit() {
            if let Some(res) = self.try_time(start, pos) {
                return res;
            }
        }
        // Fraction — but not `..` (range) and not `.method`.
        if self.at(pos) == b'.' && self.at(pos + 1).is_ascii_digit() {
            is_float = true;
            pos += 1;
            while pos < self.bytes.len() && (self.at(pos).is_ascii_digit() || self.at(pos) == b'_')
            {
                pos += 1;
            }
        }
        // Exponent.
        if matches!(self.at(pos), b'e' | b'E') {
            let mut p = pos + 1;
            if matches!(self.at(p), b'+' | b'-') {
                p += 1;
            }
            if self.at(p).is_ascii_digit() {
                is_float = true;
                pos = p;
                while pos < self.bytes.len() && self.at(pos).is_ascii_digit() {
                    pos += 1;
                }
            }
        }
        let num_text: String = self.src[start..pos].chars().filter(|&c| c != '_').collect();

        // Maximal munch: unit suffix folds into a single size/duration literal.
        let unit_start = pos;
        let mut upos = pos;
        while upos < self.bytes.len() && self.at(upos).is_ascii_alphabetic() {
            upos += 1;
        }
        let unit = &self.src[unit_start..upos];
        if !unit.is_empty() {
            // Units are lowercase (site/content/internals/language-conformance-contract.md). Reject non-canonical case rather than
            // silently folding `KB`→`kb` (decimal footgun) (D12).
            if unit.bytes().any(|b| b.is_ascii_uppercase()) {
                return Err(LexError::new(
                    format!("unit `{unit}` must be lowercase"),
                    Span::new(start, upos),
                )
                .hint("sizes: b kb mb gb tb kib mib gib tib; durations: ns us ms s m h d w"));
            }
            let lower = unit.to_ascii_lowercase();
            const SIZE_UNITS: &[&str] = &["b", "kb", "mb", "gb", "tb", "kib", "mib", "gib", "tib"];
            const DUR_UNITS: &[&str] = &["ns", "us", "ms", "s", "m", "h", "d", "w"];
            if SIZE_UNITS.contains(&lower.as_str()) {
                let word = format!("{num_text}{lower}");
                let v = shoal_value_parse_size(&word)
                    .ok_or_else(|| LexError::new("invalid size literal", Span::new(start, upos)))?;
                return Ok((Tok::Size(v), Span::new(start, upos)));
            }
            if DUR_UNITS.contains(&lower.as_str()) {
                let word = format!("{num_text}{lower}");
                let v = shoal_value_parse_duration(&word).ok_or_else(|| {
                    LexError::new("invalid duration literal", Span::new(start, upos))
                })?;
                return Ok((Tok::Duration(v), Span::new(start, upos)));
            }
            return Err(LexError::new(
                format!("unknown unit `{unit}` on numeric literal"),
                Span::new(start, upos),
            )
            .hint("sizes: b kb mb gb tb kib mib gib tib; durations: ns us ms s m h d w"));
        }

        if is_float {
            let v: f64 = num_text
                .parse()
                .map_err(|_| LexError::new("invalid float literal", Span::new(start, pos)))?;
            Ok((Tok::Float(v), Span::new(start, pos)))
        } else {
            let v: i64 = num_text.parse().map_err(|_| {
                LexError::new("integer literal out of range", Span::new(start, pos))
            })?;
            Ok((Tok::Int(v), Span::new(start, pos)))
        }
    }

    pub(crate) fn try_time(&self, start: usize, colon: usize) -> Option<LexResult> {
        let hour_txt = &self.src[start..colon];
        if hour_txt.len() > 2 || hour_txt.contains('_') {
            return None;
        }
        let hour: u8 = hour_txt.parse().ok()?;
        let mut pos = colon + 1;
        let min_start = pos;
        while pos < self.bytes.len() && self.at(pos).is_ascii_digit() {
            pos += 1;
        }
        if pos - min_start != 2 {
            return None;
        }
        let min: u8 = self.src[min_start..pos].parse().ok()?;
        let mut sec: u8 = 0;
        if self.at(pos) == b':' && self.at(pos + 1).is_ascii_digit() {
            let sec_start = pos + 1;
            let mut p = sec_start;
            while p < self.bytes.len() && self.at(p).is_ascii_digit() {
                p += 1;
            }
            if p - sec_start == 2 {
                sec = self.src[sec_start..p].parse().ok()?;
                pos = p;
            } else {
                return None;
            }
        }
        let mut hour = hour;
        // am/pm suffix
        let rest = &self.src[pos..];
        if rest.len() >= 2 {
            let suf = &rest[..2].to_ascii_lowercase();
            if suf == "am" || suf == "pm" {
                if hour == 0 || hour > 12 {
                    return None;
                }
                if suf == "pm" && hour != 12 {
                    hour += 12;
                }
                if suf == "am" && hour == 12 {
                    hour = 0;
                }
                pos += 2;
            }
        }
        if hour > 23 || min > 59 || sec > 59 {
            return Some(Err(LexError::new(
                "invalid time literal",
                Span::new(start, pos),
            )));
        }
        Some(Ok((Tok::Time { hour, min, sec }, Span::new(start, pos))))
    }
}

// Local copies of the unit parsers (shoal-syntax depends only on shoal-ast;
// keep the tiny parsing logic in sync with shoal-value::parse_size/duration).
fn shoal_value_parse_size(word: &str) -> Option<u64> {
    let split = word.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = word.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1_048_576.0,
        "gib" => 1_073_741_824.0,
        "tib" => 1_099_511_627_776.0,
        _ => return None,
    };
    if num < 0.0 {
        return None;
    }
    Some((num * mult).round() as u64)
}

fn shoal_value_parse_duration(word: &str) -> Option<i64> {
    let split = word.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = word.split_at(split);
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
    Some((num * ns).round() as i64)
}
