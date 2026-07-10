//! MCP stdio facade for the shoal kernel protocol.

use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub socket: PathBuf,
    pub session: Option<String>,
    pub token: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let socket = std::env::var_os("SHOAL_SOCKET")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|p| PathBuf::from(p).join("shoal/default.sock"))
            })
            .ok_or("set SHOAL_SOCKET or XDG_RUNTIME_DIR")?;
        Ok(Self {
            socket,
            session: std::env::var("SHOAL_SESSION").ok(),
            token: std::env::var("SHOAL_TOKEN").ok(),
        })
    }
}

pub struct KernelClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl KernelClient {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        let stream = UnixStream::connect(&config.socket)?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
        };
        client.call(
            "session.attach",
            json!({
                "session": config.session,
                "token": config.token,
                "client": {"kind":"mcp", "tty":false}
            }),
        )?;
        Ok(client)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, BridgeError> {
        let id = self.next_id;
        self.next_id += 1;
        write_json_line(
            &mut self.writer,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        loop {
            let frame = read_json_line(&mut self.reader)?.ok_or(BridgeError::Disconnected)?;
            // Kernel notifications can be interleaved with the response.
            if frame.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = frame.get("error") {
                return Err(BridgeError::Kernel(error.clone()));
            }
            return frame.get("result").cloned().ok_or_else(|| {
                BridgeError::Protocol("kernel response has neither result nor error".into())
            });
        }
    }
}

#[derive(Debug)]
pub enum BridgeError {
    Io(io::Error),
    Json(serde_json::Error),
    Protocol(String),
    Kernel(Value),
    Disconnected,
}
impl From<io::Error> for BridgeError {
    fn from(v: io::Error) -> Self {
        Self::Io(v)
    }
}
impl From<serde_json::Error> for BridgeError {
    fn from(v: serde_json::Error) -> Self {
        Self::Json(v)
    }
}
impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "{e}"),
            Self::Protocol(e) => write!(f, "{e}"),
            Self::Kernel(e) => write!(f, "kernel error: {e}"),
            Self::Disconnected => write!(f, "kernel disconnected"),
        }
    }
}
impl std::error::Error for BridgeError {}

pub struct Facade {
    kernel: KernelClient,
}
impl Facade {
    pub fn connect(config: &Config) -> Result<Self, BridgeError> {
        Ok(Self {
            kernel: KernelClient::connect(config)?,
        })
    }
    pub fn handle(&mut self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        // MCP notifications intentionally have no response.
        let id = id?;
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(rpc_error(id, -32600, "invalid JSON-RPC request", None));
        }
        let result = match method {
            Some("initialize") => Ok(
                json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"shoal-mcp","version":env!("CARGO_PKG_VERSION")}}),
            ),
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(json!({"tools":tools()})),
            Some("tools/call") => {
                self.tools_call(request.get("params").cloned().unwrap_or(Value::Null))
            }
            Some(m) => {
                return Some(rpc_error(
                    id,
                    -32601,
                    "method not found",
                    Some(json!({"method":m})),
                ));
            }
            None => {
                return Some(rpc_error(
                    id,
                    -32600,
                    "request method must be a string",
                    None,
                ));
            }
        };
        Some(match result {
            Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
            Err(e) => rpc_error(id, -32602, &e, None),
        })
    }

    fn tools_call(&mut self, params: Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tools/call requires name")?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let (method, kparams) = map_tool(name, args)?;
        match self.kernel.call(method, kparams) {
            Ok(result) => Ok(tool_result(result, false)),
            Err(BridgeError::Kernel(error)) => Ok(tool_result(error, true)),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub fn run_stdio(config: &Config) -> Result<(), BridgeError> {
    let mut facade = Facade::connect(config)?;
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    loop {
        match read_json_line(&mut reader) {
            Ok(Some(request)) => {
                if let Some(response) = facade.handle(&request) {
                    write_json_line(&mut writer, &response)?
                }
            }
            Ok(None) => return Ok(()),
            Err(BridgeError::Json(error)) => write_json_line(
                &mut writer,
                &rpc_error(
                    Value::Null,
                    -32700,
                    "parse error",
                    Some(json!({"detail":error.to_string()})),
                ),
            )?,
            Err(error) => return Err(error),
        }
    }
}

fn map_tool(name: &str, args: Value) -> Result<(&'static str, Value), String> {
    let object = args.as_object().ok_or("tool arguments must be an object")?;
    Ok(match name {
        "shoal_exec" => (
            "exec",
            json!({"src":required_str(object,"src")?,"mode":"run","position":object.get("position").and_then(Value::as_str).unwrap_or("value"),"capture":object.get("capture").cloned().unwrap_or_else(||json!({})),"timeout":object.get("timeout")}),
        ),
        "shoal_plan" => (
            "exec",
            json!({"src":required_str(object,"src")?,"mode":"plan","position":"value"}),
        ),
        "shoal_apply" => (
            "plan.apply",
            json!({"plan_ref":required_str(object,"plan_ref")?}),
        ),
        "shoal_get" => (
            "value.get",
            json!({"ref":required_str(object,"ref")?,"path":object.get("path"),"slice":object.get("slice")}),
        ),
        "shoal_journal" => ("journal.query", args),
        _ => return Err(format!("unknown tool {name:?}")),
    })
}
fn required_str<'a>(o: &'a serde_json::Map<String, Value>, name: &str) -> Result<&'a str, String> {
    o.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument {name:?}"))
}
fn tool_result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".into());
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":is_error})
}
fn rpc_error(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}})
}

pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "shoal_exec",
            "Execute shoal source and return a stable transcript reference",
            json!({"type":"object","properties":{"src":{"type":"string"},"position":{"enum":["stmt","value"]},"capture":{"type":"object"},"timeout":{"type":"number"}},"required":["src"],"additionalProperties":false}),
        ),
        tool(
            "shoal_plan",
            "Derive concrete effects without spawning",
            json!({"type":"object","properties":{"src":{"type":"string"}},"required":["src"],"additionalProperties":false}),
        ),
        tool(
            "shoal_apply",
            "Apply a previously approved plan",
            json!({"type":"object","properties":{"plan_ref":{"type":"string"}},"required":["plan_ref"],"additionalProperties":false}),
        ),
        tool(
            "shoal_get",
            "Query or slice a transcript value without re-execution",
            json!({"type":"object","properties":{"ref":{"type":"string"},"path":{"type":"string"},"slice":{"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":2}},"required":["ref"],"additionalProperties":false}),
        ),
        tool(
            "shoal_journal",
            "Query the structured execution journal",
            json!({"type":"object","properties":{"since":{"type":"integer"},"until":{"type":"integer"},"principal":{"type":"string"},"effects":{"type":"array","items":{"type":"string"}},"head":{"type":"string"},"limit":{"type":"integer","minimum":1}},"additionalProperties":false}),
        ),
    ]
}
fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input_schema})
}

fn read_json_line<R: BufRead>(reader: &mut R) -> Result<Option<Value>, BridgeError> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_FRAME {
        return Err(BridgeError::Protocol("frame exceeds 16 MiB".into()));
    }
    Ok(Some(serde_json::from_str(line.trim_end())?))
}
fn write_json_line<W: Write>(writer: &mut W, value: &Value) -> Result<(), BridgeError> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn socket_exists(path: &Path) -> bool {
    fs_type(path).is_some_and(|t| t.is_socket())
}

fn fs_type(path: &Path) -> Option<std::fs::FileType> {
    std::fs::metadata(path).ok().map(|m| m.file_type())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;
    fn mock() -> (tempfile::TempDir, Config, thread::JoinHandle<Vec<Value>>) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("kernel.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let h = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut w = stream;
            let mut seen = vec![];
            for n in 0..2 {
                let req = read_json_line(&mut r).unwrap().unwrap();
                seen.push(req.clone());
                let id = req["id"].clone();
                let result = if n == 0 {
                    json!({"session":"s","principal":"human","caps":{},"cwd":{"display":"/tmp"},"env_hash":"x","ast_version":1})
                } else {
                    json!({"ref":"out:1","value":{"$":"int","v":3}})
                };
                write_json_line(&mut w, &json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap()
            }
            seen
        });
        let c = Config {
            socket: path,
            session: Some("s".into()),
            token: Some("tok".into()),
        };
        (d, c, h)
    }
    #[test]
    fn lists_five_tools() {
        assert_eq!(tools().len(), 5);
        for t in tools() {
            assert_eq!(t["inputSchema"]["type"], "object")
        }
    }
    #[test]
    fn facade_attaches_and_maps_exec() {
        let (_d, c, h) = mock();
        let mut f = Facade::connect(&c).unwrap();
        let response=f.handle(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"shoal_exec","arguments":{"src":"1+2"}}})).unwrap();
        assert_eq!(response["result"]["structuredContent"]["ref"], "out:1");
        let seen = h.join().unwrap();
        assert_eq!(seen[0]["method"], "session.attach");
        assert_eq!(seen[0]["params"]["token"], "tok");
        assert_eq!(seen[1]["method"], "exec");
        assert_eq!(seen[1]["params"]["mode"], "run");
    }
    #[test]
    fn maps_all_tools() {
        assert_eq!(
            map_tool("shoal_plan", json!({"src":"rm x"})).unwrap().0,
            "exec"
        );
        assert_eq!(
            map_tool("shoal_apply", json!({"plan_ref":"plan:x"}))
                .unwrap()
                .0,
            "plan.apply"
        );
        assert_eq!(
            map_tool("shoal_get", json!({"ref":"out:1"})).unwrap().0,
            "value.get"
        );
        assert_eq!(
            map_tool("shoal_journal", json!({"limit":2})).unwrap().0,
            "journal.query"
        );
    }
    #[test]
    fn protocol_errors_are_structured() {
        let (_d, c, h) = mock();
        let mut f = Facade::connect(&c).unwrap();
        let e = f
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"nope"}))
            .unwrap();
        assert_eq!(e["error"]["code"], -32601);
        drop(f);
        let _ = h.join();
    }
    #[test]
    fn socket_probe_is_truthful() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        assert!(!socket_exists(&p));
        let _l = UnixListener::bind(&p).unwrap();
        assert!(socket_exists(&p));
    }
}
