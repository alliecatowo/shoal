//! Supporting scalar/handle payloads — `GlobVal`, `RegexVal`, `TimeVal`,
//! `RangeVal`, `ClosureVal`, `SecretVal` — plus the bind-time word-parsing
//! helpers (`parse_size`/`parse_duration`/`parse_time`, TDD §4.2 site 2), moved
//! verbatim out of `lib.rs`.

use super::*;

#[derive(Debug, Clone, PartialEq)]
pub struct GlobVal {
    pub pattern: String,
    /// Origin cwd — expansion always happens against this (TDD §4.3).
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

impl RangeVal {
    pub fn iter(&self) -> impl Iterator<Item = i64> + Send + use<> {
        let (start, end, inclusive) = (self.start, self.end, self.inclusive);
        let last = if inclusive {
            end
        } else {
            end.saturating_sub(1)
        };
        start..=last
    }
    pub fn len(&self) -> usize {
        let last = if self.inclusive {
            self.end
        } else {
            self.end - 1
        };
        if last < self.start {
            0
        } else {
            (last - self.start + 1) as usize
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
// Word parsing helpers (bind-time coercion, TDD §4.2 site 2)
// ---------------------------------------------------------------------------

/// Parse a size word like `1.5gb`, `4kib`, `237b`. Decimal units and binary
/// (`*ib`) units per TDD §2.1.
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
