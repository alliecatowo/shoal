use super::*;

#[test]
fn strip_ansi_removes_sgr_color_codes() {
    assert_eq!(strip_ansi("\x1b[32mhi\x1b[0m"), "hi");
}

#[test]
fn strip_ansi_removes_non_sgr_csi_sequences_too() {
    // Cursor-movement/erase CSI (final bytes `H`/`K`), not just SGR
    // color (`m`) — the stripper covers the general CSI grammar.
    assert_eq!(strip_ansi("\x1b[2K\x1b[1;1Hhello"), "hello");
}

#[test]
fn strip_ansi_is_a_no_op_on_plain_text() {
    assert_eq!(
        strip_ansi("plain text, no escapes here"),
        "plain text, no escapes here"
    );
}

/// On the headless/MCP path, the kernel strips ANSI from the human-facing
/// `render` string before it reaches the wire. A genuine interactive client
/// keeps color. The structured `value` field is unchanged in either case.
#[test]
fn headless_client_gets_ansi_stripped_render_but_a_tty_client_keeps_color() {
    // `render_block` genuinely emits ANSI for a table, so the wire assertions
    // below cannot pass vacuously.
    let mut row = shoal_value::Record::new();
    row.insert("n".to_string(), Value::Int(1));
    let raw = shoal_value::render::render_block(&Value::Table(vec![row]), 80);
    assert!(
        raw.contains('\u{1b}'),
        "sanity: render_block must emit ANSI for a table: {raw:?}"
    );

    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
    )
    .result
    .unwrap();
    let render = exec["render"].as_str().unwrap();
    assert!(
        !render.contains('\u{1b}'),
        "headless render must be ANSI-free: {render:?}"
    );
    assert!(
        render.contains('1'),
        "content must still be present: {render:?}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();

    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"human","tty":true}}),
    );
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
    )
    .result
    .unwrap();
    let render = exec["render"].as_str().unwrap();
    assert!(
        render.contains('\u{1b}'),
        "a real tty client must keep its color: {render:?}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// `value.get` with `format=render` observes the same headless contract as
/// inline `exec` rendering.
#[test]
fn headless_value_get_format_render_is_also_ansi_stripped() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
    )
    .result
    .unwrap();
    let value_ref = exec["ref"].as_str().unwrap().to_owned();
    let rendered = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref": value_ref, "format": "render"}),
    )
    .result
    .unwrap();
    let render = rendered["render"].as_str().unwrap();
    assert!(
        !render.contains('\u{1b}'),
        "headless format=render must be ANSI-free: {render:?}"
    );
    assert!(render.contains('1'), "content preserved: {render:?}");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}
