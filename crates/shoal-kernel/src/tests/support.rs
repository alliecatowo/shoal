use super::*;

pub(super) fn serve_embedded_test_stream(
    kernel: Arc<Kernel>,
    stream: UnixStream,
) -> io::Result<()> {
    kernel.handle_stream_with_trust(stream, ConnectionTrust::EmbeddedHuman)
}

pub(super) fn call(
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

pub(super) fn read_all_events(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    mut id: i64,
    channel: &str,
    since: Option<u64>,
) -> Vec<Json> {
    let mut cursor = since;
    let mut all = Vec::new();
    loop {
        let response = call(
            writer,
            reader,
            id,
            "events.read",
            json!({"channel":channel,"since":cursor,"limit":usize::MAX}),
        );
        id += 1;
        let result = response.result.expect("events.read page");
        all.extend(result["events"].as_array().unwrap().iter().cloned());
        if !result["page"]["truncated"].as_bool().unwrap() {
            break;
        }
        cursor = result["page"]["next_since"].as_u64();
        assert!(cursor.is_some(), "a truncated non-empty page has a cursor");
    }
    all
}
pub(super) fn recv_line(reader: &mut BufReader<UnixStream>) -> Json {
    let mut line = String::new();
    std::io::BufRead::read_line(reader, &mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

pub(super) fn attach(client: &mut UnixStream, reader: &mut BufReader<UnixStream>) -> Response {
    call(
        client,
        reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    )
}

pub(super) fn create_local_human_token(state_dir: &Path) -> String {
    let mut tokens = TokenStore::open(state_dir.join("tokens.json")).unwrap();
    tokens
        .create(principal(), "local-human".into(), vec![], None)
        .unwrap()
        .0
}

pub(super) fn attach_bearer(
    client: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    token: &str,
) -> Response {
    call(
        client,
        reader,
        1,
        "session.attach",
        json!({"token":token,"client":{"kind":"native-human","tty":false}}),
    )
}

pub(super) fn spawn(
    kernel: &Arc<Kernel>,
) -> (
    UnixStream,
    BufReader<UnixStream>,
    std::thread::JoinHandle<()>,
) {
    let (client, server) = UnixStream::pair().unwrap();
    let reader = BufReader::new(client.try_clone().unwrap());
    let k = kernel.clone();
    let thread = std::thread::spawn(move || serve_embedded_test_stream(k, server).unwrap());
    (client, reader, thread)
}
