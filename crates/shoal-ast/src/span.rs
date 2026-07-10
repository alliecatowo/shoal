use serde::{Deserialize, Serialize};

/// Byte-offset range into the source buffer a node was parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start: start as u32, end: end as u32 }
    }

    /// Smallest span covering both.
    pub fn join(self, other: Span) -> Span {
        Span { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    pub fn slice<'a>(&self, src: &'a str) -> &'a str {
        src.get(self.start as usize..self.end as usize).unwrap_or("")
    }
}
