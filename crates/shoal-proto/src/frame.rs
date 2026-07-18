use crate::Request;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, BufRead, Write};

/// Maximum size of one newline-delimited JSON-RPC frame.
///
/// The limit is applied while reading and while serializing, so neither side
/// buffers an arbitrarily large or unterminated frame.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
/// Maximum nested JSON arrays/objects admitted before `serde_json` allocates a
/// tree. This is below serde_json's own recursion ceiling and is symmetric on
/// request and response frames.
pub const MAX_JSON_DEPTH: usize = 64;
/// Maximum JSON values (container and scalar nodes) in one frame.
pub const MAX_JSON_NODES: usize = 65_536;
/// Maximum members/items retained by any one JSON object or array.
pub const MAX_JSON_CONTAINER_ITEMS: usize = 16_384;
/// Maximum decoded UTF-8 bytes in an object key. Ordinary value strings, such
/// as source and rendered result payloads, may still occupy the whole frame.
pub const MAX_JSON_KEY_BYTES: usize = 64 * 1024;
/// Numeric tokens cannot usefully approach the frame wall, and asking the
/// number parser to inspect a multi-megabyte scalar is pure availability cost.
pub const MAX_JSON_NUMBER_BYTES: usize = 1_024;

#[derive(Clone, Copy)]
enum State {
    ArrayFirst,
    ArrayNext,
    ArrayAfterValue,
    ObjectFirstKey,
    ObjectNextKey,
    ObjectColon,
    ObjectValue,
    ObjectAfterValue,
}

#[derive(Clone, Copy)]
struct Container {
    state: State,
    items: usize,
}

const EMPTY_CONTAINER: Container = Container {
    state: State::ArrayFirst,
    items: 0,
};

#[derive(Debug, PartialEq, Eq)]
enum PreflightError {
    Syntax(&'static str),
    Complexity(&'static str),
}

impl PreflightError {
    fn into_io(self) -> io::Error {
        let message = match self {
            Self::Syntax(reason) => format!("invalid JSON-RPC JSON syntax: {reason}"),
            Self::Complexity(reason) => {
                format!("JSON-RPC frame complexity limit exceeded: {reason}")
            }
        };
        io::Error::new(io::ErrorKind::InvalidData, message)
    }
}

/// Validate JSON grammar and allocation-driving complexity without allocating
/// a JSON tree. The scanner uses a fixed-depth stack and does not copy strings.
pub fn validate_json_frame(bytes: &[u8]) -> io::Result<()> {
    preflight(bytes).map_err(PreflightError::into_io)
}

pub fn read_json_frame<R: BufRead, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    // The content wall excludes the line terminator. Two extra bytes admit an
    // exact-limit frame with CRLF while still detecting an oversized or
    // unterminated body in bounded space.
    let mut limited = std::io::Read::take(&mut *reader, MAX_FRAME_LEN as u64 + 2);
    let count = limited.read_line(&mut line)?;
    if count == 0 {
        return Ok(None);
    }
    let body = line.trim_end_matches(['\r', '\n']);
    if body.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON-RPC frame exceeds 16 MiB",
        ));
    }
    validate_json_frame(body.as_bytes())?;
    serde_json::from_str(body).map(Some).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid JSON-RPC frame envelope",
        )
    })
}

pub fn read_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<Request>> {
    read_json_frame(reader)
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, frame: &T) -> io::Result<()> {
    let mut bytes = BoundedBuffer::new();
    if let Err(error) = serde_json::to_writer(&mut bytes, frame) {
        if bytes.overflowed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON-RPC frame exceeds 16 MiB",
            ));
        }
        return Err(io::Error::other(error));
    }
    validate_json_frame(&bytes.bytes)?;
    writer.write_all(&bytes.bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl BoundedBuffer {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(8 * 1024),
            overflowed: false,
        }
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.len() > MAX_FRAME_LEN.saturating_sub(self.bytes.len()) {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON-RPC frame exceeds 16 MiB",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn preflight(bytes: &[u8]) -> Result<(), PreflightError> {
    if std::str::from_utf8(bytes).is_err() {
        return Err(PreflightError::Syntax("invalid UTF-8"));
    }
    let mut stack = [EMPTY_CONTAINER; MAX_JSON_DEPTH];
    let mut depth = 0_usize;
    let mut index = 0_usize;
    let mut nodes = 0_usize;
    let mut root_done = false;

    loop {
        skip_whitespace(bytes, &mut index);
        if depth == 0 {
            if root_done {
                return if index == bytes.len() {
                    Ok(())
                } else {
                    Err(PreflightError::Syntax("trailing content"))
                };
            }
            if index == bytes.len() {
                return Err(PreflightError::Syntax("missing root value"));
            }
            root_done = true;
            start_value(bytes, &mut index, &mut stack, &mut depth, &mut nodes)?;
            continue;
        }

        let top = depth - 1;
        match stack[top].state {
            State::ArrayFirst => {
                if bytes.get(index) == Some(&b']') {
                    index += 1;
                    depth -= 1;
                } else {
                    admit_item(&mut stack[top])?;
                    stack[top].state = State::ArrayAfterValue;
                    start_value(bytes, &mut index, &mut stack, &mut depth, &mut nodes)?;
                }
            }
            State::ArrayNext => {
                admit_item(&mut stack[top])?;
                stack[top].state = State::ArrayAfterValue;
                start_value(bytes, &mut index, &mut stack, &mut depth, &mut nodes)?;
            }
            State::ArrayAfterValue => match bytes.get(index) {
                Some(b',') => {
                    index += 1;
                    stack[top].state = State::ArrayNext;
                }
                Some(b']') => {
                    index += 1;
                    depth -= 1;
                }
                _ => return Err(PreflightError::Syntax("expected array comma or close")),
            },
            State::ObjectFirstKey => {
                if bytes.get(index) == Some(&b'}') {
                    index += 1;
                    depth -= 1;
                } else {
                    parse_key(bytes, &mut index, &mut stack[top])?;
                }
            }
            State::ObjectNextKey => parse_key(bytes, &mut index, &mut stack[top])?,
            State::ObjectColon => {
                if bytes.get(index) != Some(&b':') {
                    return Err(PreflightError::Syntax("expected object colon"));
                }
                index += 1;
                stack[top].state = State::ObjectValue;
            }
            State::ObjectValue => {
                stack[top].state = State::ObjectAfterValue;
                start_value(bytes, &mut index, &mut stack, &mut depth, &mut nodes)?;
            }
            State::ObjectAfterValue => match bytes.get(index) {
                Some(b',') => {
                    index += 1;
                    stack[top].state = State::ObjectNextKey;
                }
                Some(b'}') => {
                    index += 1;
                    depth -= 1;
                }
                _ => return Err(PreflightError::Syntax("expected object comma or close")),
            },
        }
    }
}

fn admit_item(container: &mut Container) -> Result<(), PreflightError> {
    container.items += 1;
    if container.items > MAX_JSON_CONTAINER_ITEMS {
        return Err(PreflightError::Complexity("too many container items"));
    }
    Ok(())
}

fn parse_key(
    bytes: &[u8],
    index: &mut usize,
    container: &mut Container,
) -> Result<(), PreflightError> {
    if bytes.get(*index) != Some(&b'"') {
        return Err(PreflightError::Syntax("object key must be a string"));
    }
    admit_item(container)?;
    let decoded_bytes = parse_string(bytes, index)?;
    if decoded_bytes > MAX_JSON_KEY_BYTES {
        return Err(PreflightError::Complexity("object key is too large"));
    }
    container.state = State::ObjectColon;
    Ok(())
}

fn start_value(
    bytes: &[u8],
    index: &mut usize,
    stack: &mut [Container; MAX_JSON_DEPTH],
    depth: &mut usize,
    nodes: &mut usize,
) -> Result<(), PreflightError> {
    *nodes += 1;
    if *nodes > MAX_JSON_NODES {
        return Err(PreflightError::Complexity("too many JSON values"));
    }
    match bytes.get(*index).copied() {
        Some(b'{') => {
            *index += 1;
            push_container(stack, depth, State::ObjectFirstKey)
        }
        Some(b'[') => {
            *index += 1;
            push_container(stack, depth, State::ArrayFirst)
        }
        Some(b'"') => {
            parse_string(bytes, index)?;
            Ok(())
        }
        Some(b't') => parse_literal(bytes, index, b"true"),
        Some(b'f') => parse_literal(bytes, index, b"false"),
        Some(b'n') => parse_literal(bytes, index, b"null"),
        Some(b'-' | b'0'..=b'9') => parse_number(bytes, index),
        Some(_) => Err(PreflightError::Syntax("expected JSON value")),
        None => Err(PreflightError::Syntax("unexpected end of input")),
    }
}

fn push_container(
    stack: &mut [Container; MAX_JSON_DEPTH],
    depth: &mut usize,
    initial: State,
) -> Result<(), PreflightError> {
    if *depth == MAX_JSON_DEPTH {
        return Err(PreflightError::Complexity(
            "maximum structural depth exceeded",
        ));
    }
    stack[*depth] = Container {
        state: initial,
        items: 0,
    };
    *depth += 1;
    Ok(())
}

fn parse_literal(bytes: &[u8], index: &mut usize, literal: &[u8]) -> Result<(), PreflightError> {
    if bytes.get(*index..*index + literal.len()) != Some(literal) {
        return Err(PreflightError::Syntax("invalid literal"));
    }
    *index += literal.len();
    Ok(())
}

fn parse_number(bytes: &[u8], index: &mut usize) -> Result<(), PreflightError> {
    let start = *index;
    if bytes.get(*index) == Some(&b'-') {
        *index += 1;
    }
    match bytes.get(*index) {
        Some(b'0') => {
            *index += 1;
            if bytes.get(*index).is_some_and(u8::is_ascii_digit) {
                return Err(PreflightError::Syntax("number has a leading zero"));
            }
        }
        Some(b'1'..=b'9') => {
            *index += 1;
            while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
                *index += 1;
            }
        }
        _ => return Err(PreflightError::Syntax("invalid number")),
    }
    if bytes.get(*index) == Some(&b'.') {
        *index += 1;
        let fraction = *index;
        while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
            *index += 1;
        }
        if *index == fraction {
            return Err(PreflightError::Syntax("invalid number fraction"));
        }
    }
    if matches!(bytes.get(*index), Some(b'e' | b'E')) {
        *index += 1;
        if matches!(bytes.get(*index), Some(b'+' | b'-')) {
            *index += 1;
        }
        let exponent = *index;
        while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
            *index += 1;
        }
        if *index == exponent {
            return Err(PreflightError::Syntax("invalid number exponent"));
        }
    }
    if *index - start > MAX_JSON_NUMBER_BYTES {
        return Err(PreflightError::Complexity("numeric token is too large"));
    }
    Ok(())
}

fn parse_string(bytes: &[u8], index: &mut usize) -> Result<usize, PreflightError> {
    debug_assert_eq!(bytes.get(*index), Some(&b'"'));
    *index += 1;
    let mut decoded_bytes = 0_usize;
    loop {
        let Some(byte) = bytes.get(*index).copied() else {
            return Err(PreflightError::Syntax("unterminated string"));
        };
        match byte {
            b'"' => {
                *index += 1;
                return Ok(decoded_bytes);
            }
            0x00..=0x1f => return Err(PreflightError::Syntax("unescaped control in string")),
            b'\\' => {
                *index += 1;
                match bytes.get(*index).copied() {
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                        *index += 1;
                        decoded_bytes += 1;
                    }
                    Some(b'u') => {
                        *index += 1;
                        let first = parse_hex_quad(bytes, index)?;
                        if (0xd800..=0xdbff).contains(&first) {
                            if bytes.get(*index..*index + 2) != Some(b"\\u") {
                                return Err(PreflightError::Syntax("unpaired Unicode surrogate"));
                            }
                            *index += 2;
                            let second = parse_hex_quad(bytes, index)?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return Err(PreflightError::Syntax("unpaired Unicode surrogate"));
                            }
                            decoded_bytes += 4;
                        } else if (0xdc00..=0xdfff).contains(&first) {
                            return Err(PreflightError::Syntax("unpaired Unicode surrogate"));
                        } else {
                            decoded_bytes += char::from_u32(u32::from(first))
                                .expect("non-surrogate u16 is a Unicode scalar")
                                .len_utf8();
                        }
                    }
                    _ => return Err(PreflightError::Syntax("invalid string escape")),
                }
            }
            _ => {
                *index += 1;
                decoded_bytes += 1;
            }
        }
    }
}

fn parse_hex_quad(bytes: &[u8], index: &mut usize) -> Result<u16, PreflightError> {
    let Some(quad) = bytes.get(*index..*index + 4) else {
        return Err(PreflightError::Syntax("incomplete Unicode escape"));
    };
    let mut value = 0_u16;
    for byte in quad {
        value = value
            .checked_mul(16)
            .expect("four hexadecimal digits fit u16")
            + match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a' + 10),
                b'A'..=b'F' => u16::from(byte - b'A' + 10),
                _ => return Err(PreflightError::Syntax("invalid Unicode escape")),
            };
    }
    *index += 4;
    Ok(value)
}

fn skip_whitespace(bytes: &[u8], index: &mut usize) {
    while matches!(bytes.get(*index), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        *index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JSONRPC, Response};
    use serde_json::{Value, json};
    use std::io::Cursor;

    fn null_array(items: usize) -> String {
        let mut json = String::with_capacity(items.saturating_mul(5).saturating_add(2));
        json.push('[');
        for index in 0..items {
            if index != 0 {
                json.push(',');
            }
            json.push_str("null");
        }
        json.push(']');
        json
    }

    #[test]
    fn depth_and_container_item_boundaries_are_exact() {
        let exact_depth = format!(
            "{}null{}",
            "[".repeat(MAX_JSON_DEPTH),
            "]".repeat(MAX_JSON_DEPTH)
        );
        validate_json_frame(exact_depth.as_bytes()).unwrap();
        let too_deep = format!(
            "{}null{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        );
        let error = validate_json_frame(too_deep.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("complexity limit"));

        validate_json_frame(null_array(MAX_JSON_CONTAINER_ITEMS).as_bytes()).unwrap();
        let error =
            validate_json_frame(null_array(MAX_JSON_CONTAINER_ITEMS + 1).as_bytes()).unwrap_err();
        assert!(error.to_string().contains("too many container items"));
    }

    #[test]
    fn total_node_limit_catches_wide_multi_container_documents() {
        let array = null_array(14_000);
        let body = format!("[{array},{array},{array},{array},{array}]");
        let error = validate_json_frame(body.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("too many JSON values"));
    }

    #[test]
    fn key_limit_counts_decoded_escapes_and_value_strings_keep_frame_budget() {
        let key = "a".repeat(MAX_JSON_KEY_BYTES);
        validate_json_frame(format!("{{\"{key}\":null}}").as_bytes()).unwrap();
        let error = validate_json_frame(format!("{{\"{key}a\":null}}").as_bytes()).unwrap_err();
        assert!(error.to_string().contains("object key is too large"));

        let escaped_key = "\\u0061".repeat(MAX_JSON_KEY_BYTES);
        validate_json_frame(format!("{{\"{escaped_key}\":null}}").as_bytes()).unwrap();
        let error = validate_json_frame(format!("{{\"{escaped_key}\\u0061\":null}}").as_bytes())
            .unwrap_err();
        assert!(error.to_string().contains("object key is too large"));
    }

    #[test]
    fn escaped_delimiters_and_unicode_surrogates_are_lexed_as_string_content() {
        for body in [
            r#"{"fake\"}],{[":"still a string","slash":"\\","unicode":"\u005b\u007d"}"#,
            r#"{"emoji":"\ud83d\ude80","controls":"\b\f\n\r\t"}"#,
        ] {
            validate_json_frame(body.as_bytes()).unwrap();
            serde_json::from_str::<Value>(body).unwrap();
        }
        for malformed in [
            br#"{"x":"\q"}"#.as_slice(),
            br#"{"x":"\ud800"}"#.as_slice(),
            br#"{"x":"\ud800\u0041"}"#.as_slice(),
            br#"{"x":"\u12xz"}"#.as_slice(),
            b"{\"x\":\"unterminated}".as_slice(),
            b"{\"x\":\"\xff\"}".as_slice(),
        ] {
            let error = validate_json_frame(malformed).unwrap_err();
            assert!(error.to_string().contains("JSON syntax"), "{error}");
        }
    }

    #[test]
    fn exact_frame_sized_source_string_is_preserved() {
        let prefix = r#"{"jsonrpc":"2.0","id":1,"method":"parse","params":{"src":""#;
        let suffix = r#""}}"#;
        let source_len = MAX_FRAME_LEN - prefix.len() - suffix.len();
        let body = format!("{prefix}{}{suffix}", "x".repeat(source_len));
        assert_eq!(body.len(), MAX_FRAME_LEN);
        let mut framed = body.into_bytes();
        framed.push(b'\n');
        let request: Request = read_json_frame(&mut Cursor::new(framed)).unwrap().unwrap();
        assert_eq!(request.params["src"].as_str().unwrap().len(), source_len);
    }

    #[test]
    fn near_frame_wall_null_array_is_rejected_before_tree_allocation() {
        let items = (MAX_FRAME_LEN - 2) / 5;
        let body = null_array(items);
        assert!(body.len() <= MAX_FRAME_LEN);
        assert!(body.len() > MAX_FRAME_LEN - 16);
        let error = validate_json_frame(body.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("complexity limit"));
    }

    #[test]
    fn complexity_failure_consumes_its_line_and_next_frame_remains_readable() {
        let bad = null_array(MAX_JSON_CONTAINER_ITEMS + 1);
        let good = json!({"jsonrpc":JSONRPC,"id":7,"method":"parse","params":{}});
        let mut bytes = bad.into_bytes();
        bytes.push(b'\n');
        bytes.extend_from_slice(serde_json::to_string(&good).unwrap().as_bytes());
        bytes.push(b'\n');
        let mut reader = Cursor::new(bytes);
        let error = read_json_frame::<_, Value>(&mut reader).unwrap_err();
        assert!(error.to_string().contains("complexity limit"));
        assert_eq!(read_json_frame(&mut reader).unwrap(), Some(good));
    }

    #[test]
    fn syntax_and_complexity_errors_are_stable_and_do_not_echo_input() {
        let secret = "secret-that-must-not-be-echoed";
        let syntax = validate_json_frame(format!("{{\"{secret}\":]}}").as_bytes()).unwrap_err();
        assert!(syntax.to_string().contains("JSON syntax"));
        assert!(!syntax.to_string().contains(secret));

        let complexity =
            validate_json_frame(null_array(MAX_JSON_CONTAINER_ITEMS + 1).as_bytes()).unwrap_err();
        assert!(complexity.to_string().contains("complexity limit"));
    }

    #[test]
    fn write_side_rejects_oversized_and_overcomplex_frames_before_output() {
        let mut output = Vec::new();
        let oversized = "x".repeat(MAX_FRAME_LEN + 1);
        let error = write_frame(&mut output, &oversized).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(output.is_empty());

        let response = Response::ok(
            Value::from(1),
            Value::Array(vec![Value::Null; MAX_JSON_CONTAINER_ITEMS + 1]),
        );
        let error = write_frame(&mut output, &response).unwrap_err();
        assert!(error.to_string().contains("complexity limit"));
        assert!(output.is_empty());
    }

    #[test]
    fn oversized_numeric_tokens_are_complexity_failures() {
        let number = "1".repeat(MAX_JSON_NUMBER_BYTES + 1);
        let error = validate_json_frame(number.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("numeric token is too large"));
    }
}
