//! Long-lived Unix-socket host for the shoal evaluator (TDD §10).

use serde_json::{Value as Json, json};
use shoal_eval::Evaluator;
use shoal_proto::*;
use shoal_value::Value;
use std::collections::HashMap;
use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct Kernel {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_client: AtomicU64,
}

struct Session {
    evaluator: Mutex<Evaluator>,
    transcript: Mutex<HashMap<Ref, Value>>,
    client_it: Mutex<HashMap<u64, Ref>>,
    next_value: AtomicU64,
}

impl Kernel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
        })
    }

    pub fn serve(self: Arc<Self>, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(path)?;
        for stream in listener.incoming() {
            let kernel = self.clone();
            match stream {
                Ok(stream) => {
                    std::thread::spawn(move || {
                        let _ = kernel.handle_stream(stream);
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn handle_stream(&self, stream: UnixStream) -> io::Result<()> {
        let client = self.next_client.fetch_add(1, Ordering::Relaxed);
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        let mut attached: Option<Arc<Session>> = None;
        while let Some(request) = read_frame(&mut reader)? {
            let id = request.id.clone();
            let response = if request.jsonrpc != JSONRPC {
                Response::err(id, -32600, "invalid JSON-RPC version", None)
            } else {
                self.dispatch(request, client, &mut attached)
            };
            write_frame(&mut writer, &response)?;
        }
        Ok(())
    }

    fn dispatch(
        &self,
        request: Request,
        client: u64,
        attached: &mut Option<Arc<Session>>,
    ) -> Response {
        let id = request.id;
        let result: Result<Json, RpcError> = (|| match request.method.as_str() {
            "session.attach" => {
                let params: AttachParams = decode(request.params)?;
                let name = params.session.unwrap_or_else(|| "default".into());
                let session = self.session(&name).map_err(internal)?;
                let cwd = session
                    .evaluator
                    .lock()
                    .unwrap()
                    .cwd()
                    .as_os_str()
                    .to_owned();
                *attached = Some(session);
                encode(AttachResult {
                    session: name,
                    principal: format!("uid:{}", unsafe { libc_geteuid() }),
                    caps: json!({"enforced":false}),
                    cwd: WirePath::encode(&cwd),
                    env_hash: "local".into(),
                    ast_version: 1,
                })
            }
            "parse" => {
                let params: ParseParams = decode(request.params)?;
                let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                    code: -32001,
                    message: e.msg,
                    data: Some(json!({"span":e.span,"hint":e.hint})),
                })?;
                encode(json!({"ast_version":1,"ast":ast}))
            }
            "exec" => {
                let session = attached.as_ref().ok_or_else(not_attached)?;
                let params: ExecParams = decode(request.params)?;
                if params.mode != "run" {
                    return Err(RpcError {
                        code: -32003,
                        message: "plan mode is not implemented".into(),
                        data: None,
                    });
                }
                let ast = shoal_syntax::parse(&params.src).map_err(|e| RpcError {
                    code: -32001,
                    message: e.msg,
                    data: Some(json!({"span":e.span,"hint":e.hint})),
                })?;
                let mut evaluator = session.evaluator.lock().unwrap();
                evaluator.interactive = false;
                let value = evaluator.eval_program(&ast).map_err(|e| RpcError { code: -32002, message: e.msg, data: Some(json!({"code":e.code,"span":e.span,"hint":e.hint,"status":e.status,"stderr":e.stderr})) })?;
                let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
                session
                    .transcript
                    .lock()
                    .unwrap()
                    .insert(value_ref.clone(), value.clone());
                session
                    .client_it
                    .lock()
                    .unwrap()
                    .insert(client, value_ref.clone());
                encode(ExecResult {
                    r#ref: value_ref,
                    value: Some(wire_value(&value)),
                    render: Some(shoal_value::render::render_block(&value, 80)),
                })
            }
            "value.get" => {
                let session = attached.as_ref().ok_or_else(not_attached)?;
                let params: ValueGetParams = decode(request.params)?;
                let values = session.transcript.lock().unwrap();
                let value = values.get(&params.r#ref).ok_or_else(|| RpcError {
                    code: -32004,
                    message: "unknown value ref".into(),
                    data: None,
                })?;
                let mut wire = wire_value(value);
                if let (Some([start, end]), WireValue::List { v: items }) =
                    (params.slice, &mut wire)
                {
                    *items = items
                        .get(start.min(items.len())..end.min(items.len()))
                        .unwrap_or(&[])
                        .to_vec();
                }
                encode(json!({"ref":params.r#ref,"value":wire}))
            }
            "task.list" => encode(json!([])),
            _ => Err(RpcError {
                code: -32601,
                message: "method not found".into(),
                data: None,
            }),
        })();
        match result {
            Ok(value) => Response::ok(id, value),
            Err(error) => Response {
                jsonrpc: JSONRPC.into(),
                id,
                result: None,
                error: Some(error),
            },
        }
    }

    fn session(&self, name: &str) -> io::Result<Arc<Session>> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(name) {
            return Ok(session.clone());
        }
        let cwd = std::env::current_dir()?;
        let session = Arc::new(Session {
            evaluator: Mutex::new(Evaluator::new(cwd)),
            transcript: Mutex::new(HashMap::new()),
            client_it: Mutex::new(HashMap::new()),
            next_value: AtomicU64::new(1),
        });
        sessions.insert(name.into(), session.clone());
        Ok(session)
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Json) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError {
        code: -32602,
        message: e.to_string(),
        data: None,
    })
}
fn encode<T: serde::Serialize>(value: T) -> Result<Json, RpcError> {
    serde_json::to_value(value).map_err(internal)
}
fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: -32603,
        message: error.to_string(),
        data: None,
    }
}
fn not_attached() -> RpcError {
    RpcError {
        code: -32000,
        message: "attach to a session first".into(),
        data: None,
    }
}
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

fn wire_value(value: &Value) -> WireValue {
    match value {
        Value::Null => WireValue::Null,
        Value::Bool(v) => WireValue::Bool { v: *v },
        Value::Int(v) => WireValue::Int { v: *v },
        Value::Float(v) => WireValue::Float { v: *v },
        Value::Str(v) => WireValue::Str { v: v.clone() },
        Value::Path(v) => {
            let p = WirePath::encode(v.as_os_str());
            WireValue::Path {
                v: p.display,
                raw: p.raw,
            }
        }
        Value::Size(v) => WireValue::Size { v: *v },
        Value::Duration(v) => WireValue::Duration { v: *v },
        Value::Bytes(v) => WireValue::Bytes {
            v: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &**v),
        },
        Value::List(v) => WireValue::List {
            v: v.iter().map(wire_value).collect(),
        },
        _ => WireValue::Str {
            v: shoal_value::render::render_inline(value),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(
        writer: &mut UnixStream,
        reader: &mut BufReader<UnixStream>,
        id: i64,
        method: &str,
        params: Json,
    ) -> Response {
        write_frame(
            writer,
            &Request {
                jsonrpc: JSONRPC.into(),
                id: id.into(),
                method: method.into(),
                params,
            },
        )
        .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(reader, &mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    #[test]
    fn unix_stream_session_roundtrip() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let kernel = Kernel::new();
        let thread = std::thread::spawn(move || kernel.handle_stream(server).unwrap());
        assert!(
            call(
                &mut client,
                &mut reader,
                1,
                "session.attach",
                json!({"client":{"kind":"test","tty":false}})
            )
            .error
            .is_none()
        );
        assert!(
            call(&mut client, &mut reader, 2, "parse", json!({"src":"1 + 2"}))
                .error
                .is_none()
        );
        let exec = call(&mut client, &mut reader, 3, "exec", json!({"src":"1 + 2"}));
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let get = call(
            &mut client,
            &mut reader,
            4,
            "value.get",
            json!({"ref":value_ref,"path":null,"slice":null}),
        );
        assert_eq!(get.result.unwrap()["value"]["v"], 3);
        assert!(
            call(&mut client, &mut reader, 5, "task.list", json!({}))
                .error
                .is_none()
        );
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }
}
