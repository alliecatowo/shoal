//! Conformance corpus harness — shoal-eval's copy.
//!
//! This mirrors `crates/shoal/tests/conformance.rs`'s collect-all approach:
//! every case in `spec/cases/*.toml` runs to completion regardless of
//! earlier failures, and a single summary line (`conformance: P passed,
//! F failed, S skipped`) is printed before any panic, so a run always
//! reports the full picture instead of aborting at the first mismatch.

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

    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let suite: Suite =
            toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        for case in suite.case {
            if case.skip.is_some() {
                skipped += 1;
                continue;
            }
            let label = format!("{}::{}", file.display(), case.name);
            match run_case(&file, &case) {
                Ok(()) => passed += 1,
                Err(msg) => failures.push(format!("{label}: {msg}")),
            }
        }
    }

    let ran = passed + failures.len();
    let failed = failures.len();
    println!(
        "conformance: {passed} passed, {failed} failed, {skipped} skipped (of {} total cases)",
        ran + skipped
    );

    assert!(
        ran >= 75,
        "normative corpus must exercise at least 75 non-skipped cases; ran {ran}"
    );

    if !failures.is_empty() {
        panic!(
            "{failed} conformance case(s) failed:\n  {}",
            failures.join("\n  ")
        );
    }
}

/// Run one case to completion; `Ok(())` means it conformed to its
/// expectation, `Err(msg)` carries a human-readable mismatch description.
/// Unlike the old version of this harness, this never panics on a per-case
/// mismatch — callers collect the `Err` and keep going so the whole corpus
/// gets exercised in a single pass.
fn run_case(file: &Path, case: &Case) -> Result<(), String> {
    let temp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    for fixture in case.fixture.as_deref().unwrap_or_default() {
        let path = temp.path().join(fixture);
        if fixture.ends_with('/') {
            fs::create_dir_all(&path).map_err(|e| format!("fixture mkdir: {e}"))?;
        } else {
            fs::create_dir_all(path.parent().unwrap())
                .map_err(|e| format!("fixture mkdir: {e}"))?;
            fs::write(path, []).map_err(|e| format!("fixture write: {e}"))?;
        }
    }

    let parsed = shoal_syntax::parse(&case.src);
    if case.parse_error.unwrap_or(false) {
        return match parsed {
            Err(err) => {
                if let Some(needle) = &case.parse_error_contains
                    && !err.to_string().contains(needle.as_str())
                {
                    return Err(format!(
                        "parse_error message {:?} does not contain {:?}",
                        err.to_string(),
                        needle
                    ));
                }
                Ok(())
            }
            Ok(_) => Err(format!(
                "{}::{}: expected parse error, but the source parsed successfully",
                file.display(),
                case.name
            )),
        };
    }
    let program = match parsed {
        Ok(p) => p,
        Err(e) => return Err(format!("unexpected parse error: {e}")),
    };

    let result = Evaluator::new(temp.path().to_path_buf()).eval_program(&program);
    if let Some(code) = &case.error {
        return match result {
            Err(err) => {
                if &err.code != code {
                    return Err(format!(
                        "expected error code `{code}`, got `{}` ({})",
                        err.code, err.msg
                    ));
                }
                if let Some(needle) = &case.error_contains
                    && !err.msg.contains(needle.as_str())
                    && !err.hint.as_deref().unwrap_or("").contains(needle.as_str())
                {
                    return Err(format!(
                        "error message+hint {:?}/{:?} does not contain {:?}",
                        err.msg, err.hint, needle
                    ));
                }
                Ok(())
            }
            Ok(value) => Err(format!(
                "expected error `{code}`, but evaluation succeeded with value {:?}",
                render_inline(&value)
            )),
        };
    }

    match result {
        Ok(value) => {
            let expected = case
                .value
                .as_deref()
                .ok_or_else(|| "case needs value/error/parse_error".to_string())?;
            let rendered = render_inline(&value);
            if rendered == expected {
                Ok(())
            } else {
                Err(format!(
                    "value mismatch: expected {expected:?}, got {rendered:?}"
                ))
            }
        }
        Err(e) => Err(format!("unexpected eval error: {} (span {:?})", e, e.span)),
    }
}
