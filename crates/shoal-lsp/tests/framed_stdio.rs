use serde_json::{Value, json};
use shoal_lsp::transport::MAX_LSP_BODY_BYTES;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_lsp() -> Child {
    Command::new(env!("CARGO_BIN_EXE_shoal-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn write_frame(writer: &mut impl Write, value: &Value) {
    let body = serde_json::to_vec(value).unwrap();
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    writer.write_all(&body).unwrap();
    writer.flush().unwrap();
}

fn read_frame(reader: &mut impl BufRead) -> Value {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn wait_for_exit(child: &mut Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn real_stdio_transport_supports_an_ordinary_editor_lifecycle() {
    let mut child = spawn_lsp();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    write_frame(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"processId":null,"rootUri":null,"capabilities":{}}
        }),
    );
    let initialized = read_frame(&mut stdout);
    assert_eq!(initialized["id"], 1);
    assert!(initialized.get("result").is_some());

    write_frame(
        &mut stdin,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    );
    write_frame(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "method":"textDocument/didOpen",
            "params":{"textDocument":{
                "uri":"file:///tmp/editor-flow.shl",
                "languageId":"shoal",
                "version":1,
                "text":"let alpha = 1\nalpha"
            }}
        }),
    );
    let diagnostics = (0..4)
        .map(|_| read_frame(&mut stdout))
        .find(|frame| frame["method"] == "textDocument/publishDiagnostics")
        .expect("analysis must publish diagnostics");
    assert_eq!(diagnostics["params"]["version"], 1);

    write_frame(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"textDocument/documentSymbol",
            "params":{"textDocument":{"uri":"file:///tmp/editor-flow.shl"}}
        }),
    );
    let symbols = read_frame(&mut stdout);
    assert_eq!(symbols["id"], 3);
    assert_eq!(symbols["result"][0]["name"], "alpha");

    write_frame(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
    );
    let shutdown = read_frame(&mut stdout);
    assert_eq!(shutdown["id"], 2);
    write_frame(
        &mut stdin,
        &json!({"jsonrpc":"2.0","method":"exit","params":null}),
    );
    drop(stdin);
    assert!(wait_for_exit(&mut child));
}

#[test]
fn real_stdio_transport_rejects_absurd_content_length_without_a_body() {
    let mut child = spawn_lsp();
    let mut stdin = child.stdin.take().unwrap();
    write!(stdin, "Content-Length: {}\r\n\r\n", MAX_LSP_BODY_BYTES + 1).unwrap();
    stdin.flush().unwrap();
    assert!(
        wait_for_exit(&mut child),
        "shoal-lsp waited for an already-rejected body"
    );
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("input transport closed"));
    assert!(!stderr.contains(&(MAX_LSP_BODY_BYTES + 1).to_string()));
}
