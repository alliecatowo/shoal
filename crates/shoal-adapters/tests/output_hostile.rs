use shoal_adapters::{
    MAX_PARSE_CELL_BYTES, MAX_PARSE_HINT_BYTES, MAX_PARSE_INPUT_BYTES, MAX_PARSE_JSON_DEPTH,
    MAX_PARSE_JSON_NODES, MAX_PARSE_ROWS, parse_output,
};

#[test]
fn hostile_json_and_ndjson_depth_width_numbers_and_strings_degrade() {
    let deep = format!(
        "{}0{}",
        "[".repeat(MAX_PARSE_JSON_DEPTH + 1),
        "]".repeat(MAX_PARSE_JSON_DEPTH + 1)
    );
    assert_eq!(parse_output("json", deep.as_bytes(), None), None);
    assert_eq!(parse_output("ndjson", deep.as_bytes(), None), None);

    let wide = format!("[{}]", "0,".repeat(MAX_PARSE_JSON_NODES));
    assert_eq!(parse_output("json", wide.as_bytes(), None), None);
    let too_many_ndjson_rows = "0\n".repeat(MAX_PARSE_ROWS + 1);
    assert_eq!(
        parse_output("ndjson", too_many_ndjson_rows.as_bytes(), None),
        None
    );
    assert_eq!(
        parse_output("json", u64::MAX.to_string().as_bytes(), None),
        None
    );

    let giant = format!("\"{}\"", "x".repeat(MAX_PARSE_CELL_BYTES + 1));
    assert_eq!(parse_output("json", giant.as_bytes(), None), None);
}

#[test]
fn delimiter_floods_giant_metadata_and_malformed_text_degrade() {
    let oversized_input = vec![b'x'; MAX_PARSE_INPUT_BYTES + 1];
    assert_eq!(parse_output("lines", &oversized_input, None), None);
    let too_many_rows = "\n".repeat(1_000_000);
    assert_eq!(parse_output("lines", too_many_rows.as_bytes(), None), None);

    let delimiters = vec![b','; 1_000_000];
    assert_eq!(parse_output("csv", &delimiters, None), None);
    let nuls = vec![0; 1_000_000];
    assert_eq!(
        parse_output("z-records", &nuls, Some("table<{x: str}>")),
        None
    );

    let giant_hint = "x".repeat(MAX_PARSE_HINT_BYTES + 1);
    assert_eq!(parse_output("tsv", b"x\ny\n", Some(&giant_hint)), None);
    let giant_cell = format!("name\n{}\n", "x".repeat(MAX_PARSE_CELL_BYTES + 1));
    assert_eq!(parse_output("csv", giant_cell.as_bytes(), None), None);
    assert_eq!(parse_output("csv", b"a\n\"unterminated\n", None), None);
    assert_eq!(parse_output("csv", b"a\nq\"uote\n", None), None);
    assert_eq!(parse_output("lines", b"ok\n\xff", None), None);

    // Repeating a large header name into every row would amplify a tiny input
    // into a much larger retained table without aggregate accounting.
    let header = "h".repeat(1024);
    let amplified = format!("{header}\n{}", "x\n".repeat(40_000));
    assert_eq!(parse_output("csv", amplified.as_bytes(), None), None);
}

#[test]
fn nonfinite_and_overflowing_typed_numbers_degrade() {
    let float_hint = "table<{value: float}>";
    assert_eq!(
        parse_output("tsv-headerless", b"inf\n", Some(float_hint)),
        None
    );
    assert_eq!(
        parse_output("tsv-headerless", b"NaN\n", Some(float_hint)),
        None
    );
    let size_hint = "table<{value: size_kb}>";
    assert_eq!(
        parse_output("tsv-headerless", b"1e400\n", Some(size_hint)),
        None
    );
    assert_eq!(
        parse_output("tsv-headerless", b"18446744073709551615\n", Some(size_hint)),
        None
    );
}

#[test]
fn duplicate_headers_and_keys_never_create_partial_structures() {
    assert_eq!(parse_output("csv", b"a,a\n1,2\n", None), None);
    assert_eq!(parse_output("kv", b"a=1\na=2\n", None), None);
    assert_eq!(parse_output("csv", b"a,b\n1,2\n3\n", None), None);
    assert_eq!(
        parse_output("csv", b"actual\nvalue\n", Some("table<{promised: str}>")),
        None
    );
    assert_eq!(
        parse_output("tsv-headerless", b"value\n", Some("not-a-table-hint")),
        None
    );
    assert_eq!(
        parse_output(
            "tsv-headerless",
            b"value\n",
            Some("table<{value: mystery}>")
        ),
        None
    );
}
