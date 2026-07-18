use super::*;

fn args(value: Value) -> CallArgs {
    CallArgs {
        pos: vec![value],
        named: Vec::new(),
    }
}

#[test]
fn limited_writer_never_partially_crosses_its_wall() {
    let mut writer = LimitedWriter::new(4);
    writer.write_all(b"1234").unwrap();
    assert!(writer.write_all(b"5").is_err());
    assert_eq!(writer.bytes, b"1234");
    assert!(writer.exceeded);
}

#[test]
fn input_and_tree_limits_are_typed() {
    let oversized = "x".repeat(MAX_DATA_INPUT_BYTES + 1);
    let error = parse_json_text(&oversized, "json.parse").unwrap_err();
    assert_eq!(error.code, "data_materialization_limit");

    let many = serde_json::Value::Array(
        (0..MAX_DATA_NODES)
            .map(|_| serde_json::Value::Null)
            .collect(),
    );
    let error = admit_json_tree(&many, "json.parse").unwrap_err();
    assert_eq!(error.code, "data_materialization_limit");
    assert!(error.msg.contains("nodes"));
}

#[test]
fn http_projection_does_not_clone_oversized_json() {
    let oversized = " ".repeat(MAX_DATA_INPUT_BYTES + 1);
    assert_eq!(http_json_projection(&oversized), Value::Null);
}

#[test]
fn json_writer_and_toml_preflight_reject_escape_amplification() {
    let amplifying = Value::Str("\0".repeat(3 * 1024 * 1024));
    let error = stringify_json(&args(amplifying.clone())).unwrap_err();
    assert_eq!(error.code, "data_materialization_limit");
    assert!(error.msg.contains("output"));

    let mut record = Record::new();
    record.insert("payload".into(), amplifying);
    let error = stringify_toml(&args(Value::Record(record))).unwrap_err();
    assert_eq!(error.code, "data_materialization_limit");
    assert!(error.msg.contains("output"));
}

#[test]
fn csv_parser_stops_at_the_row_wall() {
    let mut source = String::from("value\n");
    for _ in 0..=MAX_CSV_ROWS {
        source.push_str("x\n");
    }
    let error = parse_csv(&args(Value::Str(source))).unwrap_err();
    assert_eq!(error.code, "data_materialization_limit");
    assert!(error.msg.contains("row"));
}
