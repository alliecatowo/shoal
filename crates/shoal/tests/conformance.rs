//! Conformance corpus harness (WP4).
//!
//! Walks `spec/cases/*.toml` (schema pinned in `site/content/internals/intercrate-protocol-contracts.md`) and,
//! for each case, builds a fresh `shoal_eval::Evaluator` rooted at a fresh
//! temp-dir cwd containing the case's `fixture` entries, parses `src` with
//! the script-mode parser (`shoal_syntax::parse`), evaluates it, and renders
//! the final value with `shoal_value::render::render_inline`.
//!
//! The corpus is normative (site/content/internals/language-conformance-contract.md: "the corpus decides disputes").
//! Cases encode the CORRECT behavior per site/content/internals/language-conformance-contract.md + site/content/internals/intercrate-protocol-contracts.md,
//! not necessarily what the current implementation does — this harness is
//! expected to have failures while shoal-syntax/shoal-eval are still being
//! built out in parallel. See spec/README.md for the authoring guide.
//!
//! This file is a single umbrella `#[test]` so cargo doesn't explode into
//! one test per case; per-case failures are collected and reported by name.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use shoal_eval::Evaluator;
use shoal_value::render::render_inline;

/// One `[[case]]` entry from a `spec/cases/*.toml` file.
struct Case {
    file: String,
    name: String,
    src: String,
    value: Option<String>,
    error: Option<String>,
    error_contains: Option<String>,
    parse_error: bool,
    parse_error_contains: Option<String>,
    fixture: Vec<String>,
    skip: Option<String>,
}

fn toml_str(t: &toml::Value, key: &str) -> Option<String> {
    t.get(key).and_then(toml::Value::as_str).map(String::from)
}

fn toml_bool(t: &toml::Value, key: &str) -> bool {
    t.get(key).and_then(toml::Value::as_bool).unwrap_or(false)
}

fn toml_str_list(t: &toml::Value, key: &str) -> Vec<String> {
    t.get(key)
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(toml::Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Load every `[[case]]` from every `*.toml` file directly under `dir`,
/// sorted by filename so iteration order (and any process-global counters
/// touched during eval, e.g. task ids) is stable across runs.
fn load_cases(dir: &Path) -> Vec<Case> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let doc: toml::Value =
            toml::from_str(&text).unwrap_or_else(|e| panic!("parse toml {}: {e}", path.display()));
        let Some(cases) = doc.get("case").and_then(toml::Value::as_array) else {
            continue;
        };
        for c in cases {
            let name =
                toml_str(c, "name").unwrap_or_else(|| panic!("case missing `name` in {file_name}"));
            let src = toml_str(c, "src")
                .unwrap_or_else(|| panic!("case `{name}` missing `src` in {file_name}"));
            out.push(Case {
                file: file_name.clone(),
                name,
                src,
                value: toml_str(c, "value"),
                error: toml_str(c, "error"),
                error_contains: toml_str(c, "error_contains"),
                parse_error: toml_bool(c, "parse_error"),
                parse_error_contains: toml_str(c, "parse_error_contains"),
                fixture: toml_str_list(c, "fixture"),
                skip: toml_str(c, "skip"),
            });
        }
    }
    out
}

/// Materialize `fixture` entries under `root`, per site/content/internals/intercrate-protocol-contracts.md. A trailing
/// slash (`"d/"`) makes a directory; anything else is an empty file. Parent
/// dirs are auto-created. This mirrors the shoal-eval conformance harness so
/// both runners interpret the same corpus identically.
fn write_fixtures(root: &Path, fixture: &[String]) -> Result<(), String> {
    for rel in fixture {
        let p = root.join(rel);
        if rel.ends_with('/') {
            fs::create_dir_all(&p).map_err(|e| format!("fixture mkdir {rel:?}: {e}"))?;
            continue;
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("fixture mkdir for {rel:?}: {e}"))?;
        }
        fs::write(&p, b"").map_err(|e| format!("fixture write {rel:?}: {e}"))?;
    }
    Ok(())
}

/// Run one case to completion; `Ok(())` means it conformed to its
/// expectation, `Err(msg)` carries a human-readable mismatch description.
fn run_case(case: &Case) -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    write_fixtures(tmp.path(), &case.fixture)?;

    // Script-mode parse. If a dedicated script-mode entry point lands later
    // (e.g. distinguishing REPL-only forms like bare `it`/`out`), prefer it;
    // fall back to the always-available `parse`.
    let program = match shoal_syntax::parse(&case.src) {
        Ok(p) => p,
        Err(e) => {
            return if case.parse_error {
                match &case.parse_error_contains {
                    Some(sub)
                        if !e.msg.contains(sub.as_str())
                            && !e.hint.as_deref().unwrap_or("").contains(sub.as_str()) =>
                    {
                        Err(format!(
                            "parse_error message+hint {:?}/{:?} does not contain {:?}",
                            e.msg, e.hint, sub
                        ))
                    }
                    _ => Ok(()),
                }
            } else {
                Err(format!(
                    "unexpected parse error: {} (span {:?})",
                    e.msg, e.span
                ))
            };
        }
    };
    if case.parse_error {
        return Err("expected a parse error, but the source parsed successfully".to_string());
    }

    let mut evaluator = Evaluator::new(tmp.path().to_path_buf());
    let outcome = evaluator.eval_program(&program);
    // Keep the tempdir alive through eval (fixtures may be read/written);
    // drop explicitly afterward for clarity.
    let result = match outcome {
        Ok(value) => {
            if let Some(expected_code) = &case.error {
                Err(format!(
                    "expected error `{expected_code}`, but evaluation succeeded with value {:?}",
                    render_inline(&value)
                ))
            } else if let Some(expected) = &case.value {
                let rendered = render_inline(&value);
                if rendered.trim() == expected.trim() {
                    Ok(())
                } else {
                    Err(format!(
                        "value mismatch: expected {:?}, got {:?}",
                        expected.trim(),
                        rendered.trim()
                    ))
                }
            } else {
                Err("case has none of `value`/`error`/`parse_error` to check against".to_string())
            }
        }
        Err(err) => match &case.error {
            None => Err(format!(
                "unexpected eval error `{}`: {} (span {:?})",
                err.code, err.msg, err.span
            )),
            Some(expected_code) if &err.code != expected_code => Err(format!(
                "expected error code `{expected_code}`, got `{}` ({})",
                err.code, err.msg
            )),
            Some(_) => match &case.error_contains {
                // Checked against message-or-hint: teaching diagnostics (site/content/internals/language-conformance-contract.md)
                // often carry the human-readable clarification in `hint`.
                Some(sub)
                    if !err.msg.contains(sub.as_str())
                        && !err.hint.as_deref().unwrap_or("").contains(sub.as_str()) =>
                {
                    Err(format!(
                        "error message+hint {:?}/{:?} does not contain {:?}",
                        err.msg, err.hint, sub
                    ))
                }
                _ => Ok(()),
            },
        },
    };
    drop(tmp);
    result
}

#[test]
fn conformance_corpus() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let spec_dir = manifest_dir.join("../../spec/cases");
    let cases = load_cases(&spec_dir);
    assert!(
        !cases.is_empty(),
        "no conformance cases found under {}",
        spec_dir.display()
    );

    // Names must be globally unique across every file (site/content/internals/intercrate-protocol-contracts.md / spec/README.md).
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut dupes = Vec::new();
    for c in &cases {
        if let Some(prev_file) = seen.insert(c.name.clone(), c.file.clone()) {
            dupes.push(format!(
                "duplicate case name `{}` in {} and {}",
                c.name, prev_file, c.file
            ));
        }
    }

    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for case in &cases {
        if let Some(reason) = &case.skip {
            skipped += 1;
            println!(
                "conformance: SKIP {} [{}] ({})",
                case.name, case.file, reason
            );
            continue;
        }
        match run_case(case) {
            Ok(()) => passed += 1,
            Err(msg) => failures.push(format!("{} [{}]: {}", case.name, case.file, msg)),
        }
    }

    let failed = failures.len() + dupes.len();
    println!(
        "conformance: {passed} passed, {failed} failed, {skipped} skipped (of {} total cases)",
        cases.len()
    );

    if !dupes.is_empty() || !failures.is_empty() {
        let mut lines = dupes;
        lines.extend(failures);
        panic!(
            "{failed} conformance case(s) failed:\n  {}",
            lines.join("\n  ")
        );
    }
}
