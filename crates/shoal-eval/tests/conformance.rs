use serde::Deserialize;
use shoal_eval::Evaluator;
use shoal_value::render::render_inline;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
struct Suite {
    case: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    src: String,
    value: Option<String>,
    error: Option<String>,
    error_contains: Option<String>,
    parse_error: Option<bool>,
    parse_error_contains: Option<String>,
    fixture: Option<Vec<String>>,
    skip: Option<String>,
}

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/cases")
}

#[test]
fn normative_conformance_corpus() {
    let mut files: Vec<_> = fs::read_dir(cases_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no conformance suites found");
    let mut ran = 0usize;
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let suite: Suite =
            toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        for case in suite.case {
            if case.skip.is_some() {
                continue;
            }
            ran += 1;
            run_case(&file, &case);
        }
    }
    assert!(
        ran >= 75,
        "normative corpus must exercise at least 75 non-skipped cases; ran {ran}"
    );
}

fn run_case(file: &Path, case: &Case) {
    let label = format!("{}::{}", file.display(), case.name);
    let temp = tempfile::tempdir().unwrap();
    for fixture in case.fixture.as_deref().unwrap_or_default() {
        let path = temp.path().join(fixture);
        if fixture.ends_with('/') {
            fs::create_dir_all(&path).unwrap();
        } else {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, []).unwrap();
        }
    }
    let parsed = shoal_syntax::parse(&case.src);
    if case.parse_error.unwrap_or(false) {
        let err = parsed.expect_err(&format!("{label}: expected parse error"));
        if let Some(needle) = &case.parse_error_contains {
            assert!(err.to_string().contains(needle), "{label}: {err}");
        }
        return;
    }
    let program = parsed.unwrap_or_else(|e| panic!("{label}: unexpected parse error: {e}"));
    let result = Evaluator::new(temp.path().to_path_buf()).eval_program(&program);
    if let Some(code) = &case.error {
        let err = result.expect_err(&format!("{label}: expected error {code}"));
        assert_eq!(&err.code, code, "{label}: {err}");
        if let Some(needle) = &case.error_contains {
            assert!(
                err.msg.contains(needle) || err.hint.as_deref().is_some_and(|h| h.contains(needle)),
                "{label}: {err}"
            );
        }
    } else {
        let value = result.unwrap_or_else(|e| panic!("{label}: unexpected eval error: {e}"));
        assert_eq!(
            render_inline(&value),
            case.value
                .as_deref()
                .expect("case needs value/error/parse_error"),
            "{label}"
        );
    }
}
