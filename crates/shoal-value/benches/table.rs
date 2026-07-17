//! `table_1m_where_sort`: builds a real `Value::Table` and drives it through the actual
//! `shoal_value::methods::call_method` dispatcher — the same entry point `shoal-eval` calls for
//! every `.where(...)`/`.sort(...)` in the language — for a one-million-row `where` (filter) then
//! `sort` (descending by key). This is HR-F5's fix (site/content/internals/hardening-roadmap.md):
//! the previous version of this file built a bare `Vec<Record>` and filtered/sorted it with plain
//! Rust iterator code, never constructing a `Value::Table` or calling any Shoal method — it
//! measured raw `Vec<i64>` filter+sort, not table-method performance (deep audit finding I12).
//!
//! What IS measured: the real `.where`/`.sort` dispatch path — `call_method` ->
//! `methods::list::filter`/`sort_by` -> the shared `seq()` table-to-record conversion -> the
//! `ops::compare`-based total-order comparator — on 1,000,000 `{n: Int}` rows.
//!
//! What is NOT measured: closure evaluation cost. Real Shoal closures (`Value::Closure`) are
//! interpreted AST nodes owned by `shoal-eval`, which this crate cannot depend on (`shoal-value` is
//! beneath `shoal-eval` in the dependency graph; see
//! site/content/internals/intercrate-protocol-contracts.md). `BenchCallCtx` below stands in for the
//! evaluator by recognizing two fixed marker closures and computing their Rust equivalent directly
//! — the same technique `shoal-value`'s own unit tests use to exercise `filter`/`sort_by` without an
//! evaluator (see `crates/shoal-value/src/methods/mod.rs`'s `#[cfg(test)] struct C`). So this bench
//! is honest about table-method/comparator/storage overhead but does not include real closure
//! dispatch overhead, which would need a `shoal-eval`-hosted bench instead.

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use shoal_ast::Span;
use shoal_value::methods::call_method;
use shoal_value::{CallArgs, CallCtx, Fs, Record, StdFs, VResult, Value};

/// Marker closures the real language would represent as `Value::Closure`; this
/// bench-local `CallCtx` recognizes them by name and computes the Rust
/// equivalent, in place of interpreting an AST body (see module doc).
const PRED_MOD10: &str = "__bench_pred_n_mod_10_eq_0";
const KEY_NEG_N: &str = "__bench_key_neg_n";

struct BenchCallCtx;

impl CallCtx for BenchCallCtx {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value> {
        let Value::Record(r) = &args[0] else {
            unreachable!("bench rows are always records")
        };
        let Some(Value::Int(n)) = r.get("n") else {
            unreachable!("bench rows always carry an int `n` field")
        };
        match f {
            Value::Str(tag) if tag.as_str() == PRED_MOD10 => Ok(Value::Bool(n % 10 == 0)),
            // Negate so ascending `sort_by` on this key yields descending order
            // by `n` — the same `[...].sort_by(k => -k.n)` idiom the language
            // itself would use for a descending sort.
            Value::Str(tag) if tag.as_str() == KEY_NEG_N => Ok(Value::Int(-n)),
            _ => unreachable!("bench installs only the two marker closures above"),
        }
    }

    fn cwd(&self) -> PathBuf {
        PathBuf::from(".")
    }

    fn fs(&self) -> &dyn Fs {
        static STD: StdFs = StdFs;
        &STD
    }
}

fn build_table(rows: i64) -> Value {
    let records = (0..rows)
        .map(|i| {
            let mut r = Record::new();
            r.insert("n".into(), Value::Int(i));
            r
        })
        .collect::<Vec<_>>();
    Value::Table(records)
}

fn bench_suite(c: &mut Criterion) {
    let table = build_table(1_000_000);
    c.bench_function("table_1m_where_sort", |b| {
        b.iter_batched(
            || table.clone(),
            |t| {
                let mut ctx = BenchCallCtx;
                let filtered = call_method(
                    &mut ctx,
                    t,
                    "where",
                    CallArgs {
                        pos: vec![Value::Str(PRED_MOD10.into())],
                        named: vec![],
                    },
                    Span::default(),
                )
                .expect("where over a table of records must not error");
                let sorted = call_method(
                    &mut ctx,
                    filtered,
                    "sort",
                    CallArgs {
                        pos: vec![Value::Str(KEY_NEG_N.into())],
                        named: vec![],
                    },
                    Span::default(),
                )
                .expect("sort over a list of records must not error");
                black_box(sorted)
            },
            criterion::BatchSize::LargeInput,
        )
    });
}
criterion_group!(criterion_benches, bench_suite);
criterion_main!(criterion_benches);
