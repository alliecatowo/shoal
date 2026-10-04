//! Exact numeric-token admission for JSON text before value conversion.

use super::*;

fn number_range(what: &str, token: &str) -> ErrorVal {
    ErrorVal::new(
        "number_range",
        format!("{what}: number `{token}` is outside Shoal's numeric range"),
    )
    .with_hint("Shoal integers are signed 64-bit; encode larger integer identifiers as strings")
}

/// serde_json normally rounds integer tokens beyond `u64` (and negative tokens
/// below `i64`) into `f64` while building a `Value`. Scan a successfully parsed
/// document before conversion so exact token spellings remain available.
pub fn preflight_json_numbers(source: &str, what: &str) -> VResult<()> {
    let bytes = source.as_bytes();
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if !matches!(byte, b'-' | b'0'..=b'9') || !json_value_start(bytes, index) {
            index += 1;
            continue;
        }

        let start = index;
        index += 1;
        while index < bytes.len()
            && matches!(bytes[index], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        {
            index += 1;
        }
        if !json_value_end(bytes, index) {
            continue;
        }
        let token = &source[start..index];
        let Some(integer) = valid_json_number_is_integer(token.as_bytes()) else {
            continue;
        };
        if integer {
            if token.parse::<i64>().is_err() {
                return Err(number_range(what, token));
            }
        } else if token.parse::<f64>().is_ok_and(|number| !number.is_finite()) {
            return Err(number_range(what, token));
        }
    }
    Ok(())
}

fn json_value_start(bytes: &[u8], index: usize) -> bool {
    bytes[..index]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .is_none_or(|previous| matches!(bytes[previous], b'[' | b'{' | b',' | b':'))
}

fn json_value_end(bytes: &[u8], index: usize) -> bool {
    index == bytes.len()
        || matches!(
            bytes[index],
            b']' | b'}' | b',' | b' ' | b'\t' | b'\r' | b'\n'
        )
}

/// Return whether a token is an integer when it is exactly valid JSON number
/// grammar, or `None` for malformed candidates that serde_json must diagnose.
fn valid_json_number_is_integer(token: &[u8]) -> Option<bool> {
    let mut index = usize::from(token.first() == Some(&b'-'));
    match token.get(index)? {
        b'0' => index += 1,
        b'1'..=b'9' => {
            index += 1;
            while token.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
        }
        _ => return None,
    }

    let mut integer = true;
    if token.get(index) == Some(&b'.') {
        integer = false;
        index += 1;
        let start = index;
        while token.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return None;
        }
    }
    if matches!(token.get(index), Some(b'e' | b'E')) {
        integer = false;
        index += 1;
        if matches!(token.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let start = index;
        while token.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return None;
        }
    }
    (index == token.len()).then_some(integer)
}
