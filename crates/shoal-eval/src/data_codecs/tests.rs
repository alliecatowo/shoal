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
fn json_integer_tokens_are_exact_or_rejected() {
    for (source, expected) in [
        ("9007199254740992", Value::Int(9_007_199_254_740_992)),
        ("9223372036854775807", Value::Int(i64::MAX)),
        ("-9223372036854775808", Value::Int(i64::MIN)),
    ] {
        assert_eq!(parse_json_text(source, "json.parse").unwrap(), expected);
    }

    for source in [
        "9223372036854775808",
        "18446744073709551615",
        "-9223372036854775809",
    ] {
        let error = parse_json_text(source, "json.parse").unwrap_err();
        assert_eq!(error.code, "number_range", "source: {source}");
        assert!(error.msg.contains(source));
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("signed 64-bit"))
        );
    }
}

#[test]
fn json_decimal_and_exponent_tokens_remain_floats() {
    assert_eq!(
        parse_json_text("1.25", "json.parse").unwrap(),
        Value::Float(1.25)
    );
    assert_eq!(
        parse_json_text("1e3", "json.parse").unwrap(),
        Value::Float(1_000.0)
    );

    let error = parse_json_text("1e400", "json.parse").unwrap_err();
    assert_eq!(error.code, "arg_error");
}

#[test]
fn json_number_preflight_ignores_strings_and_preserves_parse_errors() {
    assert_eq!(
        parse_json_text(r#""18446744073709551615""#, "json.parse").unwrap(),
        Value::Str("18446744073709551615".into())
    );
    let error = parse_json_text("[01, 18446744073709551615]", "json.parse").unwrap_err();
    assert_eq!(error.code, "arg_error");
}

#[test]
fn structured_decoders_share_the_signed_integer_boundary() {
    let error = parse_yaml(&args(Value::Str("id: 18446744073709551615".into()))).unwrap_err();
    assert_eq!(error.code, "number_range");
    assert!(error.msg.starts_with("yaml.parse:"));
}

#[test]
fn http_json_projection_never_exposes_a_rounded_integer() {
    assert_eq!(
        http_json_projection("9223372036854775807"),
        Value::Int(i64::MAX)
    );
    assert_eq!(http_json_projection("9223372036854775808"), Value::Null);
    assert_eq!(
        http_json_projection(r#"{"id":18446744073709551615}"#),
        Value::Null
    );
    assert_eq!(http_json_projection("1e3"), Value::Float(1_000.0));
}

#[test]
fn stringify_after_json_parse_never_emits_a_rounded_substitute() {
    for source in ["9007199254740992", "9223372036854775807"] {
        let exact = parse_json_text(source, "json.parse").unwrap();
        assert_eq!(
            stringify_json(&args(exact)).unwrap(),
            Value::Str(source.into())
        );
    }
    assert!(parse_json_text("18446744073709551615", "json.parse").is_err());
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
