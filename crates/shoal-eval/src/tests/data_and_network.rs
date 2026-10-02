use super::*;

// --- match: type / record / list patterns (site/content/internals/language-conformance-contract.md) -----------------

#[test]
fn match_type_pattern_binds_and_falls_through() {
    assert_eq!(
        run(r#"match 5 { int n => "int:{n}"; _ => "other" }"#).unwrap(),
        Value::Str("int:5".into())
    );
    assert_eq!(
        run(r#"match "hi" { str s => "str:{s}"; _ => "other" }"#).unwrap(),
        Value::Str("str:hi".into())
    );
    // A type mismatch falls through to the next arm.
    assert_eq!(
        run(r#"match "hi" { int n => "int:{n}"; str s => "str:{s}" }"#).unwrap(),
        Value::Str("str:hi".into())
    );
    // A bare type name with no binder is a plain bind (matches anything).
    assert_eq!(
        run(r#"match 5 { int => 1; _ => 0 }"#).unwrap(),
        Value::Int(1)
    );
}

#[test]
fn match_record_pattern_shorthand_sub_and_open() {
    assert_eq!(
        run(r#"match {name: "ada", age: 30} { {name, age} => "{name} is {age}"; _ => "no" }"#)
            .unwrap(),
        Value::Str("ada is 30".into())
    );
    // Nested record sub-pattern.
    assert_eq!(
        run("match {point: {x: 1, y: 2}} { {point: {x, y}} => x + y; _ => 0 }").unwrap(),
        Value::Int(3)
    );
    // Missing field falls through (open matching only ignores *extra*).
    assert_eq!(
        run(r#"match {name: "ada"} { {name, age} => "has age"; _ => "no age" }"#).unwrap(),
        Value::Str("no age".into())
    );
    // Record + nested list sub-pattern.
    assert_eq!(
        run("match {items: [1, 2, 3]} { {items: [a, b, c]} => a + b + c; _ => 0 }").unwrap(),
        Value::Int(6)
    );
}

#[test]
fn match_record_pattern_guard_composes() {
    assert_eq!(
            run(r#"match {status: 200} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("ok".into())
        );
    assert_eq!(
            run(r#"match {status: 404} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("other:404".into())
        );
}

#[test]
fn match_list_pattern_arity_rest_and_empty() {
    assert_eq!(
        run("match [1, 2, 3] { [a, b, c] => a + b + c; _ => 0 }").unwrap(),
        Value::Int(6)
    );
    // `...rest` binds the tail as a list.
    assert_eq!(
        run("match [1, 2, 3, 4] { [first, ...rest] => rest.len(); _ => 0 }").unwrap(),
        Value::Int(3)
    );
    // Fixed arity: a length mismatch falls through.
    assert_eq!(
        run(r#"match [1, 2] { [a, b, c] => "three"; [a, b] => "two"; _ => "other" }"#).unwrap(),
        Value::Str("two".into())
    );
    assert_eq!(
        run(r#"match [] { [] => "empty"; _ => "nonempty" }"#).unwrap(),
        Value::Str("empty".into())
    );
    assert_eq!(
        run(r#"match [1] { [] => "empty"; [a] => "one:{a}"; _ => "other" }"#).unwrap(),
        Value::Str("one:1".into())
    );
}

#[test]
fn match_comma_separated_arms_parse() {
    assert_eq!(
        run(r#"match 2 { 1 => "a", 2 => "b", _ => "c" }"#).unwrap(),
        Value::Str("b".into())
    );
}

// --- data namespaces ------------------------------------------------------

#[test]
fn json_namespace_roundtrips() {
    assert_eq!(run(r#"json.parse('{"a":1}').a"#).unwrap(), Value::Int(1));
    assert_eq!(
        run("json.stringify([1, 2, 3])").unwrap(),
        Value::Str("[1,2,3]".into())
    );
    // A bound name shadows the namespace.
    assert_eq!(run("let json = 7\njson").unwrap(), Value::Int(7));
    // Invalid JSON is an arg_error.
    assert_eq!(
        run(r#"json.parse('{not json}')"#).unwrap_err().code,
        "arg_error"
    );
}

#[test]
fn json_number_range_errors_leave_the_evaluator_usable() {
    let mut evaluator = Evaluator::new(std::env::current_dir().unwrap());
    let program = shoal_syntax::parse("json.parse('9223372036854775808')").unwrap();
    let error = evaluator.eval_program(&program).unwrap_err();
    assert_eq!(error.code, "number_range");
    assert!(error.msg.contains("9223372036854775808"));
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("strings"))
    );

    assert_eq!(
        evaluator
            .eval_program(&shoal_syntax::parse("json.parse('9007199254740992')").unwrap())
            .unwrap(),
        Value::Int(9_007_199_254_740_992)
    );
    assert_eq!(
        evaluator
            .eval_program(&shoal_syntax::parse("40 + 2").unwrap())
            .unwrap(),
        Value::Int(42)
    );
}

#[test]
fn yaml_and_toml_and_csv_namespaces() {
    // yaml round-trips a scalar map.
    assert_eq!(run("yaml.parse('a: 1').a").unwrap(), Value::Int(1));
    // toml parses a key.
    assert_eq!(run("toml.parse('a = 1').a").unwrap(), Value::Int(1));
    // csv parses a header row into a table of records.
    let v = run(r#"csv.parse("name,age\nada,30")"#).unwrap();
    let Value::Table(rows) = v else {
        panic!("csv.parse should be a table, got {v:?}")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], Value::Str("ada".into()));
    assert_eq!(rows[0]["age"], Value::Str("30".into()));
}

#[test]
fn oversized_structured_data_is_typed_and_the_evaluator_recovers() {
    let document = format!(
        "[{}]",
        std::iter::repeat_n("null", crate::data_codecs::MAX_DATA_NODES)
            .collect::<Vec<_>>()
            .join(",")
    );
    let source = format!("json.parse('{document}')");
    let program = shoal_syntax::parse(&source).unwrap();
    let mut evaluator = Evaluator::new(std::env::current_dir().unwrap());
    let error = evaluator.eval_program(&program).unwrap_err();
    assert_eq!(error.code, "data_materialization_limit");
    assert!(error.msg.contains("nodes"));
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("stream"))
    );

    assert_eq!(
        evaluator
            .eval_program(&shoal_syntax::parse("40 + 2").unwrap())
            .unwrap(),
        Value::Int(42),
        "rejecting oversized structured data must leave the evaluator usable"
    );
}

#[test]
fn math_namespace_constants_and_fns() {
    assert_eq!(run("math.sqrt(4)").unwrap(), Value::Float(2.0));
    let Value::Float(pi) = run("math.pi").unwrap() else {
        panic!("math.pi should be a float")
    };
    assert!((pi - std::f64::consts::PI).abs() < 1e-12);
    assert_eq!(run("math.max(3, 7)").unwrap(), Value::Float(7.0));
    assert_eq!(run("math.clamp(9, 0, 5)").unwrap(), Value::Float(5.0));
    // clamp with lo > hi is an arg_error.
    assert_eq!(run("math.clamp(1, 5, 0)").unwrap_err().code, "arg_error");
}

#[test]
fn os_namespace_reports_platform() {
    assert_eq!(
        run("os.platform()").unwrap(),
        Value::Str(std::env::consts::OS.into())
    );
    assert_eq!(
        run("os.arch()").unwrap(),
        Value::Str(std::env::consts::ARCH.into())
    );
    assert_eq!(
        run("os.pid()").unwrap(),
        Value::Int(std::process::id() as i64)
    );
    assert!(matches!(run("os.cpus()").unwrap(), Value::Int(n) if n >= 1));
    assert!(matches!(run("os.env()").unwrap(), Value::Record(_)));
}

#[test]
#[ignore = "requires network access; gated out of CI"]
fn http_get_is_typed() {
    let v = run(r#"http.get("https://example.com")"#).unwrap();
    let Value::Record(r) = v else { panic!() };
    assert!(matches!(r.get("status"), Some(Value::Int(_))));
    assert!(matches!(r.get("ok"), Some(Value::Bool(_))));
    assert!(matches!(r.get("body"), Some(Value::Str(_))));
}

#[test]
fn http_redirects_never_connect_to_an_unplanned_second_authority() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let denied = TcpListener::bind("127.0.0.1:0").unwrap();
    denied.set_nonblocking(true).unwrap();
    let denied_address = denied.local_addr().unwrap();

    let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
    let redirect_address = redirect.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = redirect.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0u8; 1024];
        let read = stream.read(&mut request).unwrap();
        assert!(request[..read].starts_with(b"GET /start HTTP/1.1"));
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: http://{denied_address}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.flush().unwrap();
    });

    let response = run(&format!("http.get(\"http://{redirect_address}/start\")")).unwrap();
    server.join().unwrap();
    let Value::Record(response) = response else {
        panic!("HTTP response must remain typed")
    };
    assert_eq!(response["status"], Value::Int(302));
    assert_eq!(response["ok"], Value::Bool(false));
    assert!(
        matches!(denied.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "the redirect target received a connection absent from the effect plan"
    );

    let agent = crate::namespaces::http_agent();
    assert_eq!(agent.config().max_redirects(), 0);
    assert!(
        agent.config().proxy().is_none(),
        "ambient process proxy authority must not bypass the request plan"
    );
}
