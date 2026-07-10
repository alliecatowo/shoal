//! Shoal's newline-framed JSON-RPC 2.0 wire contract (TDD §7).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

pub const JSONRPC: &str = "2.0";
pub type RequestId = Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    pub fn ok(id: RequestId, value: impl Serialize) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            result: Some(serde_json::to_value(value).expect("serializable RPC result")),
            error: None,
        }
    }
    pub fn err(id: RequestId, code: i32, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

pub fn read_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<Request>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > 16 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON-RPC frame exceeds 16 MiB",
        ));
    }
    serde_json::from_str(line.trim_end())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, frame: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, frame).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct Ref(pub String);

impl Ref {
    pub fn new(kind: &str, id: impl std::fmt::Display) -> Self {
        Self(format!("{kind}:{id}"))
    }
    pub fn kind(&self) -> Option<&str> {
        self.0.split_once(':').map(|x| x.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "$", rename_all = "snake_case")]
pub enum WireValue {
    Null,
    Bool {
        v: bool,
    },
    Int {
        v: i64,
    },
    Float {
        v: f64,
    },
    Str {
        v: String,
    },
    Size {
        v: u64,
    },
    Duration {
        v: i64,
    },
    Bytes {
        v: String,
    },
    Path {
        v: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw: Option<String>,
    },
    List {
        v: Vec<WireValue>,
    },
    Record {
        v: serde_json::Map<String, Value>,
    },
    Ref {
        v: Ref,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WirePath {
    pub display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl WirePath {
    pub fn encode(path: &OsStr) -> Self {
        let bytes = path.as_bytes();
        match std::str::from_utf8(bytes) {
            Ok(text) => Self {
                display: text.into(),
                raw: None,
            },
            Err(_) => Self {
                display: path.to_string_lossy().into_owned(),
                raw: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            },
        }
    }
    pub fn decode(&self) -> Result<OsString, base64::DecodeError> {
        Ok(match &self.raw {
            Some(raw) => OsString::from_vec(base64::engine::general_purpose::STANDARD.decode(raw)?),
            None => OsString::from(&self.display),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientInfo {
    pub kind: String,
    pub tty: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AttachParams {
    pub session: Option<String>,
    pub token: Option<String>,
    pub client: ClientInfo,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachResult {
    pub session: String,
    pub principal: String,
    pub caps: Value,
    pub cwd: WirePath,
    pub env_hash: String,
    pub ast_version: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseParams {
    pub src: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecParams {
    pub src: String,
    #[serde(default = "run_mode")]
    pub mode: String,
    #[serde(default = "stmt_position")]
    pub position: String,
}
fn run_mode() -> String {
    "run".into()
}
fn stmt_position() -> String {
    "stmt".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub r#ref: Ref,
    pub value: Option<WireValue>,
    pub render: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueGetParams {
    pub r#ref: Ref,
    pub path: Option<String>,
    pub slice: Option<[usize; 2]>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_are_newline_delimited() {
        let response = Response::ok(Value::from(1), serde_json::json!({"ok":true}));
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &response).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let decoded: Response = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, response);
    }
    #[test]
    fn non_utf8_path_roundtrips() {
        let original = OsString::from_vec(vec![b'a', 0xff, b'b']);
        let wire = WirePath::encode(&original);
        assert!(wire.raw.is_some());
        assert_eq!(wire.decode().unwrap(), original);
    }
}
